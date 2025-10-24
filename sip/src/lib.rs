//! # SIP Protocol Implementation
//! 
//! This module provides a comprehensive implementation of the Session Initiation Protocol (SIP)
//! for the Nebula VoIP system. It handles SIP message parsing, transaction management,
//! and transport layer abstraction.
//! 
//! ## Core Components
//! 
//! - **Message Handling**: SIP message parsing, validation, and generation
//! - **Transaction Management**: SIP transaction state machine and lifecycle management
//! - **Transport Layer**: Support for UDP, TCP, TLS, and WebSocket transports
//! - **Finite State Machine**: SIP transaction state management
//! 
//! ## Architecture
//! 
//! The SIP implementation follows RFC 3261 standards and provides:
//! - Client and server transaction support
//! - Message routing and forwarding
//! - Transport layer abstraction
//! - Error handling and retry mechanisms
//! 
//! ## Usage
//! 
//! ```rust
//! use sip::transaction::TM;
//! use sip::message::Message;
//! 
//! // Create and send a SIP message
//! let message = Message::new();
//! TM.send_message(message).await?;
//! ```

pub mod fsm;
pub mod message;
pub mod tcp;
pub mod tls;
pub mod transaction;
pub mod transport;
pub mod udp;
pub mod wss;
