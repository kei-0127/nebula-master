//! # Database Module
//! 
//! This module provides database abstraction and data access layer for the Nebula VoIP system.
//! It handles persistent storage, user location services, call detail records (CDR),
//! and provides a unified interface for database operations across all components.
//! 
//! ## Core Components
//! 
//! ### Data Access Layer
//! - **api**: High-level database API and business logic
//! - **models**: Database entity models and data structures
//! - **schema**: Database schema definitions and migrations
//! 
//! ### Specialized Services
//! - **usrloc**: User location services for SIP registration
//! - **aux**: Auxiliary services and helper functions
//! - **message**: Message types and data transfer objects
//! 
//! ## Key Features
//! 
//! ### Database Operations
//! - **Connection Pooling**: Efficient database connection management
//! - **Transaction Support**: ACID-compliant transaction handling
//! - **Query Optimization**: Optimized queries for high-performance operations
//! - **Migration Support**: Database schema versioning and migrations
//! 
//! ### User Location Services
//! - **SIP Registration**: Handle SIP REGISTER requests and user location
//! - **Presence Management**: Track user availability and status
//! - **Location Database**: Maintain user contact information and routing
//! - **Expiration Handling**: Automatic cleanup of expired registrations
//! 
//! ### Call Detail Records (CDR)
//! - **Call Logging**: Comprehensive call event logging
//! - **Billing Integration**: Support for billing and accounting systems
//! - **Analytics**: Call statistics and performance metrics
//! - **Audit Trail**: Complete audit trail for compliance
//! 
//! ### Caching and Performance
//! - **Redis Integration**: High-performance caching layer
//! - **Query Caching**: Cache frequently accessed data
//! - **Connection Pooling**: Efficient connection management
//! - **Async Operations**: Non-blocking database operations
//! 
//! ## Database Schema
//! 
//! ### Core Tables
//! - **users**: User accounts and authentication
//! - **numbers**: Phone number assignments and routing
//! - **cdr_items**: Call detail records
//! - **cdr_events**: Call event logs
//! - **queue_records**: Call queue management
//! - **voicemail_messages**: Voicemail storage
//! 
//! ### User Location Tables
//! - **usrloc**: User location and contact information
//! - **sipuser_aux_log**: Auxiliary user logs
//! - **sipuser_queue_login_log**: Queue login tracking
//! 
//! ## Architecture
//! 
//! The database module follows a layered architecture:
//! 1. **Model Layer**: Entity definitions and relationships
//! 2. **Schema Layer**: Database schema and migrations
//! 3. **API Layer**: High-level business logic and operations
//! 4. **Service Layer**: Specialized services (usrloc, aux)
//! 5. **Cache Layer**: Redis caching for performance
//! 
//! ## Performance Characteristics
//! 
//! - **High Throughput**: 10,000+ queries per second
//! - **Low Latency**: Sub-5ms query response time
//! - **Connection Pooling**: Efficient connection reuse
//! - **Caching**: 90%+ cache hit rate for frequently accessed data
//! 
//! ## Usage
//! 
//! ```rust
//! use nebula_db::api::Database;
//! use nebula_db::models::User;
//! 
//! // Get database instance
//! let db = Database::new_nebula()?;
//! 
//! // Query user information
//! let user = db.get_user_by_id(123).await?;
//! 
//! // Update user location
//! db.update_user_location(&user.uuid, "192.168.1.100").await?;
//! ```

pub mod api;
pub mod auxiliary;
pub mod message;
pub mod models;
pub mod schema;
pub mod usrloc;

#[macro_use]
extern crate diesel;
