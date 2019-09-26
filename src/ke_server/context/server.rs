// This file is part of cfnts.
// Copyright (c) 2019, Cloudflare. All rights reserved.
// See LICENSE for licensing information.

//! NTS-KE server instantiation.

use crossbeam::sync::WaitGroup;

use mio::tcp::TcpListener;

use slog::info;

use std::net::ToSocketAddrs;
use std::rc::Rc;
use std::sync::{Arc, RwLock};

use crate::cfsock;
use crate::ke_server::KeServerConfig;
use crate::key_rotator::KeyRotator;
use crate::key_rotator::RotateError;
use crate::key_rotator::periodic_rotate;
use crate::metrics;
use crate::nts_ke::server::NTSKeyServer;

use super::listener::KeServerListener;

/// NTS-KE server state that will be shared among listeners.
pub struct KeServerState {
    /// Configuration for the NTS-KE server.
    // You can see that I don't expand the config's properties here because, by keeping it like
    // this, we will know what is the config and what is the state.
    pub(super) config: KeServerConfig,

    /// Key rotator. Read this property to get latest keys.
    // The internal state of this rotator can be changed even if the KeServer instance is
    // immutable. That's because of the nature of RwLock. This property is normally used by
    // KeServer to read the state only.
    pub(super) rotator: Arc<RwLock<KeyRotator>>,

    /// TLS server configuration which will be used among listeners.
    // We use `Arc` here so that every thread can read the config, but the drawback of using `Arc`
    // is that it uses garbage collection.
    pub(super) tls_server_config: Arc<rustls::ServerConfig>,
}

/// NTS-KE server instance.
pub struct KeServer {
    /// State shared among listerners.
    // We use `Rc` so that all the KeServerListener's can reference back to this object.
    state: Rc<KeServerState>,

    // In fact, you can check if the server already started or not, but checking that this vector
    // is empty.
    // TODO: Remove this when it is used.
    #[allow(dead_code)]
    listeners: Vec<KeServerListener>,
}

impl KeServer {
    /// Create a new `KeServer` instance, connect to the Memcached server, and rotate initial keys.
    ///
    /// This doesn't start the server yet. It just makes to the state that it's ready to start.
    /// Please run `start` to start the server.
    pub fn connect(config: KeServerConfig) -> Result<KeServer, RotateError> {
        let rotator = KeyRotator::connect(
            String::from("/nts/nts-keys"),
            String::from(config.memcached_url()),

            // We need to clone all of the following properties because the key rotator also
            // has to own them.
            config.cookie_key().clone(),
            config.logger().clone(),
        )?;

        // Putting it in a block just to make it easier to read :)
        let tls_server_config = {
            // No client auth for TLS server.
            let client_auth = rustls::NoClientAuth::new();
            // TLS server configuration.
            let mut server_config = rustls::ServerConfig::new(client_auth);

            // We support only TLS1.3
            server_config.versions = vec![rustls::ProtocolVersion::TLSv1_3];

            // Set the certificate chain and its corresponding private key.
            server_config
                .set_single_cert(
                    // rustls::ServerConfig wants to own both of them.
                    config.tls_certs.clone(),
                    config.tls_secret_keys[0].clone()
                )
                .expect("invalid key or certificate");

            // According to the NTS specification, ALPN protocol must be "ntske/1".
            server_config
                .set_protocols(&[Vec::from("ntske/1".as_bytes())]);

            server_config
        };

        let state = Rc::new(KeServerState {
            config,
            rotator: Arc::new(RwLock::new(rotator)),
            tls_server_config: Arc::new(tls_server_config),
        });

        Ok(KeServer {
            state,
            listeners: Vec::new(),
        })
    }

    /// Start the server.
    pub fn start(&mut self) {
        let logger = self.state.config.logger();

        // Side-effect. Logging.
        info!(logger, "initializing keys with memcached");

        // Create another reference to the lock so that we can pass it to another thread and
        // periodically rotate the keys.
        let mutable_rotator = self.state.rotator.clone();

        // Create a new thread and periodically rotate the keys.
        periodic_rotate(mutable_rotator);

        // We need to clone the metrics config here because we need to move it to another thread.
        if let Some(metrics_config) = self.state.config.metrics_config.clone() {
            info!(logger, "spawning metrics");

            // Create a child logger to use inside the metric server.
            let log_metrics = logger.new(slog::o!("component" => "metrics"));

            // Start a metric server.
            std::thread::spawn(move || {
                metrics::run_metrics(metrics_config, &log_metrics)
                    .expect("metrics could not be run; starting ntp server failed");
            });
        }

        // TODO: I will refactor the following later.

        eprintln!("config.addrs: {:?}", self.state.config.addrs());

        let wg = WaitGroup::new();

        for addr in self.state.config.addrs() {
            let addr = addr.to_socket_addrs().unwrap().next().unwrap();
            let listener = cfsock::tcp_listener(&addr).unwrap();
            eprintln!("listener: {:?}", listener);
            let mut tlsserv = NTSKeyServer::new(
                TcpListener::from_listener(listener, &addr).unwrap(),
                self.state.tls_server_config.clone(),
                self.state.rotator.clone(),
                self.state.config.next_port,
                addr,
                logger.clone(),
                self.state.config.timeout(),
            ).unwrap();
            info!(logger, "Starting NTS-KE server over TCP/TLS on {:?}", addr);
            let wg = wg.clone();
            std::thread::spawn(move || {
                tlsserv.listen_and_serve();
                drop(wg);
            });
        }

        wg.wait();
    }

    /// Return the state of the server.
    pub(super) fn state(&self) -> &Rc<KeServerState> {
        &self.state
    }
}