//! # Proxy Module
//! 
//! The Proxy module implements a SIP proxy server that handles SIP message routing,
//! user location services, and acts as the entry point for all SIP communications
//! in the Nebula VoIP system.
//! 
//! ## Core Components
//! 
//! ### SIP Processing
//! - **server**: Main proxy server implementation and SIP message handling
//! - **router**: SIP message routing logic and destination resolution
//! - **auth**: SIP authentication and authorization
//! 
//! ### User Management
//! - **presence**: User presence and availability management
//! - **provider**: External service provider integration
//! 
//! ## Key Features
//! 
//! ### SIP Proxy Functions
//! - **Message Routing**: Route SIP messages to appropriate destinations
//! - **User Location**: Maintain user location database and routing
//! - **Load Balancing**: Distribute load across multiple switchboard instances
//! - **Call State**: Track call state for proper message routing
//! - **Authentication**: Verify user credentials and permissions
//! 
//! ### User Location Services
//! - **Registration**: Handle SIP REGISTER requests
//! - **Location Database**: Maintain user location information
//! - **Routing Logic**: Determine message destinations
//! - **Presence Management**: Track user availability
//! 
//! ### Security Features
//! - **SIP Authentication**: Digest authentication support
//! - **Access Control**: IP-based access restrictions
//! - **Rate Limiting**: Protection against DoS attacks
//! - **TLS Support**: Encrypted SIP communication
//! 
//! ### Integration Features
//! - **Database Integration**: Persistent user location storage
//! - **Redis Integration**: Real-time presence and caching
//! - **External Providers**: Integration with external SIP providers
//! - **Webhook Support**: Real-time event notifications
//! 
//! ## Architecture
//! 
//! The proxy follows a stateless architecture:
//! 1. **Message Reception**: Receive SIP messages from clients
//! 2. **Authentication**: Verify user credentials
//! 3. **Routing Decision**: Determine message destination
//! 4. **Message Forwarding**: Forward messages to appropriate servers
//! 5. **Response Handling**: Process responses and route back to clients
//! 
//! ## Protocol Support
//! 
//! - **SIP/2.0**: Full SIP protocol support (RFC 3261)
//! - **UDP**: Primary transport protocol
//! - **TCP**: Reliable transport for large messages
//! - **TLS**: Encrypted transport for security
//! - **WebSocket**: Web-based SIP communication
//! 
//! ## Performance Characteristics
//! 
//! - **High Throughput**: 10,000+ SIP messages per second
//! - **Low Latency**: Sub-10ms message processing time
//! - **Scalable**: Horizontal scaling across multiple instances
//! - **Reliable**: Fault-tolerant with automatic failover
//! 
//! ## Configuration
//! 
//! The proxy is configured via `/etc/nebula/nebula.conf`:
//! - **Network settings**: IP addresses and ports
//! - **Database settings**: Connection strings and pool settings
//! - **Redis settings**: Cache configuration
//! - **Security settings**: Authentication and access control
//! 
//! ## Usage
//! 
//! ```rust
//! use nebula_proxy::server::ProxyServer;
//! 
//! // Start the proxy server
//! let mut server = ProxyServer::new();
//! server.run().await?;
//! ```

pub mod auth;
pub mod presence;
pub mod provider;
pub mod router;
pub mod server;
