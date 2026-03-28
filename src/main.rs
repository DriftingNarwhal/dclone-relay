//! DClone relay server binary.
//!
//! Reads a keypair from the `RELAY_KEYPAIR` environment variable (base64-encoded
//! protobuf), starts a libp2p swarm with circuit relay v2, rendezvous server,
//! identify, and ping behaviours, and exposes a tiny axum health-check on port
//! 4002 (or `$PORT` if set).
//!
//! Listens on TCP + QUIC for both IPv4 and IPv6.

use anyhow::{Context, Result};
use axum::{routing::get, Router};
use base64::Engine;
use futures::StreamExt;
use libp2p::{
    identify, identity, kad,
    noise, ping, relay, rendezvous,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, Multiaddr, PeerId, Swarm,
};
use std::{
    collections::HashMap,
    net::SocketAddr,
    time::{Duration, Instant},
};
use tracing::{info, warn};

// ── Behaviour ─────────────────────────────────────────────────────────────────

#[derive(NetworkBehaviour)]
struct RelayBehaviour {
    relay:      relay::Behaviour,
    rendezvous: rendezvous::server::Behaviour,
    identify:   identify::Behaviour,
    ping:       ping::Behaviour,
    kad:        kad::Behaviour<kad::store::MemoryStore>,
}

// ── Per-IP rate limiting state ────────────────────────────────────────────────

struct RateState {
    /// Reservation timestamps indexed by originating peer ID string.
    windows: HashMap<String, Vec<Instant>>,
}

impl RateState {
    fn new() -> Self {
        Self { windows: HashMap::new() }
    }

    /// Returns `true` if the peer is within the reservation rate limit
    /// (≤ 5 reservations per 60 s).
    fn allow(&mut self, peer_id_str: &str) -> bool {
        let now = Instant::now();
        let window = Duration::from_secs(60);
        let bucket = self.windows.entry(peer_id_str.to_owned()).or_default();
        bucket.retain(|t| now.duration_since(*t) < window);
        if bucket.len() >= 5 {
            return false;
        }
        bucket.push(now);
        true
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,libp2p_relay=debug".into()),
        )
        .init();

    // ── Spawn health-check HTTP server immediately so Railway can reach /health
    // even while the swarm is still initialising.
    let http_port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(4002);
    let addr = SocketAddr::from(([0, 0, 0, 0], http_port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("🌐 Health-check server on http://{}", addr);

    // peer_id is filled in once the keypair is ready; until then the endpoint
    // returns a placeholder so the healthcheck still gets HTTP 200.
    let peer_id_cell: std::sync::Arc<std::sync::RwLock<String>> =
        std::sync::Arc::new(std::sync::RwLock::new("starting".to_string()));
    let peer_id_cell_http = peer_id_cell.clone();

    let app = Router::new()
        .route(
            "/health",
            get(move || {
                let cell = peer_id_cell_http.clone();
                async move {
                    let pid = cell.read().unwrap().clone();
                    format!("{{\"peer_id\":\"{}\"}}", pid)
                }
            }),
        )
        .route("/", get(|| async { "DClone relay server" }));

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap_or_else(|e| {
            warn!("Health-check server error: {}", e);
        });
    });

    // ── Load keypair ──────────────────────────────────────────────────────────
    let keypair_b64 = std::env::var("RELAY_KEYPAIR")
        .context("RELAY_KEYPAIR environment variable is required")?;
    let keypair_bytes = base64::engine::general_purpose::STANDARD
        .decode(keypair_b64.trim())
        .context("RELAY_KEYPAIR is not valid base64")?;
    let key = identity::Keypair::from_protobuf_encoding(&keypair_bytes)
        .context("RELAY_KEYPAIR protobuf decoding failed")?;
    let local_peer_id = PeerId::from(key.public());
    info!("🆔 Relay PeerID: {}", local_peer_id);

    *peer_id_cell.write().unwrap() = local_peer_id.to_string();

    // ── Build swarm ───────────────────────────────────────────────────────────
    let swarm = build_swarm(key, local_peer_id)?;

    // ── Run swarm ─────────────────────────────────────────────────────────────
    let mut rate_state = RateState::new();
    run_swarm(swarm, &mut rate_state).await
}

// ── Swarm construction ────────────────────────────────────────────────────────

fn build_swarm(key: identity::Keypair, local_peer_id: PeerId) -> Result<Swarm<RelayBehaviour>> {
    let relay_cfg = relay::Config {
        max_reservations: 128,
        max_reservations_per_peer: 4,
        max_circuits: 32,
        max_circuit_duration: Duration::from_secs(120),
        max_circuit_bytes: 8 * 1024 * 1024,
        ..Default::default()
    };

    let key_clone = key.clone();
    let swarm = libp2p::SwarmBuilder::with_existing_identity(key)
        .with_tokio()
        .with_tcp(
            tcp::Config::default().nodelay(true),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_quic()
        .with_behaviour(|_key| {
            let relay = relay::Behaviour::new(local_peer_id, relay_cfg);

            let rendezvous = rendezvous::server::Behaviour::new(
                rendezvous::server::Config::default(),
            );

            let identify = identify::Behaviour::new(
                identify::Config::new(
                    "/dclone/identify/1.0.0".to_string(),
                    key_clone.public(),
                )
                .with_push_listen_addr_updates(true),
            );

            let ping = ping::Behaviour::new(
                ping::Config::new().with_interval(Duration::from_secs(30)),
            );

            let kad_store = kad::store::MemoryStore::new(local_peer_id);
            let kad = kad::Behaviour::new(local_peer_id, kad_store);

            RelayBehaviour { relay, rendezvous, identify, ping, kad }
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();

    Ok(swarm)
}

// ── Event loop ────────────────────────────────────────────────────────────────

async fn run_swarm(
    mut swarm: Swarm<RelayBehaviour>,
    rate_state: &mut RateState,
) -> Result<()> {
    for addr_str in &[
        "/ip4/0.0.0.0/tcp/4001",
        "/ip6/::/tcp/4001",
        "/ip4/0.0.0.0/udp/4001/quic-v1",
        "/ip6/::/udp/4001/quic-v1",
    ] {
        match addr_str.parse::<Multiaddr>() {
            Ok(addr) => {
                if let Err(e) = swarm.listen_on(addr.clone()) {
                    warn!("Could not listen on {}: {}", addr, e);
                }
            }
            Err(e) => warn!("Invalid listen address {}: {}", addr_str, e),
        }
    }

    // Graceful shutdown on CTRL-C / SIGTERM.
    let shutdown = async {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigterm = signal(SignalKind::terminate()).unwrap();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = sigterm.recv() => {},
            }
        }
        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c().await.ok();
        }
    };
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => {
                info!("Shutting down relay server.");
                return Ok(());
            }
            event = swarm.next() => {
                let event = match event {
                    Some(e) => e,
                    None => return Ok(()),
                };
                handle_event(event, rate_state);
            }
        }
    }
}

fn handle_event(event: SwarmEvent<RelayBehaviourEvent>, rate_state: &mut RateState) {
    match event {
        SwarmEvent::NewListenAddr { address, .. } => {
            info!("📡 Listening on {}", address);
        }

        SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
            info!("✅ Connected: {} ({})", peer_id, endpoint.get_remote_address());
        }

        SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
            info!("🔌 Disconnected: {} ({:?})", peer_id, cause);
        }

        SwarmEvent::Behaviour(RelayBehaviourEvent::Relay(event)) => {
            use relay::Event as RE;
            match event {
                RE::ReservationReqAccepted { src_peer_id, renewed } => {
                    let allowed = rate_state.allow(&src_peer_id.to_string());
                    if !allowed {
                        warn!("🚫 Rate-limit: reservation from {} (will expire naturally)", src_peer_id);
                    }
                    info!(
                        "🏠 Relay reservation {} by {}{}",
                        if renewed { "renewed" } else { "accepted" },
                        src_peer_id,
                        if !allowed { " [rate-limited]" } else { "" }
                    );
                }
                RE::ReservationReqDenied { src_peer_id } => {
                    warn!("❌ Relay reservation denied for {}", src_peer_id);
                }
                RE::CircuitReqAccepted { src_peer_id, dst_peer_id } => {
                    info!("🔀 Relay circuit: {} → {}", src_peer_id, dst_peer_id);
                }
                RE::CircuitReqDenied { src_peer_id, dst_peer_id } => {
                    warn!("❌ Relay circuit denied: {} → {}", src_peer_id, dst_peer_id);
                }
                RE::CircuitClosed { src_peer_id, dst_peer_id, error } => {
                    info!("🔌 Circuit closed: {} → {} ({:?})", src_peer_id, dst_peer_id, error);
                }
                _ => {}
            }
        }

        SwarmEvent::Behaviour(RelayBehaviourEvent::Rendezvous(event)) => {
            use rendezvous::server::Event as RSE;
            match event {
                RSE::PeerRegistered { peer, registration } => {
                    info!("📝 Rendezvous: {} registered in {:?}", peer, registration.namespace);
                }
                RSE::PeerUnregistered { peer, namespace } => {
                    info!("📝 Rendezvous: {} unregistered from {:?}", peer, namespace);
                }
                RSE::DiscoverServed { enquirer, registrations } => {
                    info!(
                        "🔍 Rendezvous: served {} registrations to {}",
                        registrations.len(),
                        enquirer
                    );
                }
                RSE::RegistrationExpired(reg) => {
                    info!("⏰ Rendezvous: registration expired for {:?}", reg.namespace);
                }
                _ => {}
            }
        }

        SwarmEvent::Behaviour(RelayBehaviourEvent::Identify(event)) => {
            use identify::Event as IE;
            if let IE::Received { peer_id, info } = event {
                info!(
                    "🪪 Identify: {} supports {} protocol(s)",
                    peer_id,
                    info.protocols.len()
                );
            }
        }

        SwarmEvent::Behaviour(RelayBehaviourEvent::Ping(event)) => {
            if let Err(e) = &event.result {
                warn!("⚠️  Ping failure for {}: {:?}", event.peer, e);
            }
        }

        _ => {}
    }
}
