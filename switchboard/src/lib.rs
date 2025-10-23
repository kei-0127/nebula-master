//! # Switchboard Module
//! 
//! The Switchboard is the core call control and routing engine of the Nebula VoIP system.
//! It manages call flows, routing decisions, conference rooms, and integrates with external
//! services to provide comprehensive telephony features.
//! 
//! ## Core Components
//! 
//! ### Call Management
//! - **channel**: Individual call channel management and state tracking
//! - **callflow**: Call flow logic and routing decisions
//! - **bridge**: Call bridging and conferencing capabilities
//! - **conference**: Multi-party conference room management
//! 
//! ### Queue Management
//! - **callqueue**: Call center queue management and agent routing
//! - **room**: Conference room and virtual meeting management
//! 
//! ### System Components
//! - **server**: HTTP API server and RPC endpoint management
//! - **switchboard**: Core switchboard logic and coordination
//! 
//! ## Key Features
//! 
//! ### Call Control
//! - **Call Routing**: Intelligent routing based on rules and policies
//! - **Call Bridging**: Connect multiple call legs together
//! - **Call Transfer**: Blind and attended call transfers
//! - **Call Hold**: Call hold and resume functionality
//! - **Call Recording**: Real-time call recording and playback
//! 
//! ### Conference Management
//! - **Multi-party Calls**: Support for 3+ party conferences
//! - **Room Management**: Virtual conference rooms with persistent state
//! - **Participant Control**: Mute, unmute, and participant management
//! - **Media Mixing**: Audio/video mixing for conferences
//! 
//! ### Queue Management
//! - **Agent Management**: Agent login/logout and availability
//! - **Queue Routing**: Intelligent queue routing algorithms
//! - **Queue Statistics**: Real-time queue performance metrics
//! - **Callback Management**: Scheduled callback functionality
//! 
//! ### Integration Features
//! - **External APIs**: Integration with external services
//! - **Webhook Support**: Real-time event notifications
//! - **Database Integration**: Persistent storage for call data
//! - **Redis Integration**: Real-time state management
//! 
//! ## Architecture
//! 
//! The switchboard follows a state machine architecture:
//! 1. **Call State Management**: Track call states and transitions
//! 2. **Event Processing**: Handle call events and triggers
//! 3. **Routing Engine**: Make routing decisions based on rules
//! 4. **Media Coordination**: Coordinate with media servers
//! 5. **External Integration**: Interface with external systems
//! 
//! ## API Endpoints
//! 
//! The switchboard provides REST API endpoints for:
//! - **Call Management**: Create, modify, and terminate calls
//! - **Conference Control**: Manage conference rooms and participants
//! - **Queue Operations**: Agent and queue management
//! - **System Monitoring**: Health checks and metrics
//! 
//! ## Performance Characteristics
//! 
//! - **High Concurrency**: 1000+ concurrent calls per instance
//! - **Low Latency**: Sub-100ms call setup time
//! - **Scalable**: Horizontal scaling across multiple instances
//! - **Reliable**: Fault-tolerant with automatic failover
//! 
//! ## Usage
//! 
//! ```rust
//! use nebula_switchboard::server::Server;
//! use nebula_switchboard::channel::Channel;
//! 
//! // Start the switchboard server
//! let server = Server::new().await?;
//! server.run().await?;
//! 
//! // Create a new call channel
//! let channel = Channel::new();
//! ```

pub mod bridge;
pub mod callflow;
pub mod callqueue;
pub mod channel;
pub mod conference;
pub mod room;
pub mod server;
pub mod switchboard;
