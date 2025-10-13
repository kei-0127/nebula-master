# Nebula VoIP System Architecture

# Overview
Nebula is a distributed VoIP (Voice over IP) system built in Rust, designed for high-performance real-time communication. The system follows a microservices architecture with three main components that work together to provide SIP-based telephony services.

## System Architecture

### Core Components

#### 1. Nebula Proxy (`nebula-proxy`)
**Purpose**: SIP proxy server that handles SIP message routing and user location services
- **Port**: 5060 (SIP), 8121 (RPC)
- **Responsibilities**:
  - SIP message routing and forwarding
  - User location services (registrar)
  - Call state management
  - Authentication and authorization
  - Load balancing across switchboard instances

#### 2. Nebula Switchboard (`nebula-switchboard`)
**Purpose**: Call control and routing engine
- **Port**: 8122 (RPC), HTTP API endpoints
- **Responsibilities**:
  - Call routing and call flow management
  - Conference room management
  - Queue management for call centers
  - Call recording and monitoring
  - Integration with external services

#### 3. Nebula Media (`nebula-media`)
**Purpose**: Media processing and RTP handling
- **Port**: 8123 (RPC), RTP/UDP ports
- **Responsibilities**:
  - RTP media stream processing
  - Audio/video codec handling
  - Media transcoding and mixing
  - WebRTC peer connections
  - Media recording and playback

### Supporting Components

#### Database Layer (`db/`)
- PostgreSQL database for persistent storage
- User location services
- Call detail records (CDR)
- Configuration and settings

#### Redis Layer (`redis/`)
- In-memory caching and session storage
- Message queuing
- Real-time presence information
- Distributed locks

#### RPC Communication (`rpc/`)
- Inter-service communication protocol
- JSON-RPC based messaging
- Service discovery and health checks

#### SIP Protocol (`sip/`)
- SIP message parsing and generation
- Transaction management
- Transport layer abstraction (UDP, TCP, TLS, WebSocket)

## Data Flow Architecture

### Call Establishment Flow
```
Client → Proxy → Switchboard → Media
   ↓        ↓         ↓         ↓
  SIP    Route    Control   Process
         Message   Call      Media
```

### Component Interaction
```
┌─────────────┐    ┌─────────────┐    ┌─────────────┐
│   Client    │    │    Proxy    │    │ Switchboard │
│   (SIP)     │◄──►│    (SIP)    │◄──►│    (RPC)    │
└─────────────┘    └─────────────┘    └─────────────┘
                           │                   │
                           ▼                   ▼
                   ┌─────────────┐    ┌─────────────┐
                   │   Redis     │    │    Media    │
                   │  (Cache)    │    │   (RTP)     │
                   └─────────────┘    └─────────────┘
                           │                   │
                           └───────────────────┘
                                   │
                                   ▼
                           ┌─────────────┐
                           │  Database   │
                           │ (PostgreSQL)│
                           └─────────────┘
```

## Module Structure

### Core Modules
- **`proxy/`**: SIP proxy implementation
- **`switchboard/`**: Call control and routing
- **`media/`**: Media processing and RTP handling
- **`sip/`**: SIP protocol implementation
- **`db/`**: Database abstraction layer
- **`rpc/`**: Inter-service communication
- **`redis/`**: Redis client and utilities

### Utility Modules
- **`utils/`**: Common utilities and helpers
- **`timer/`**: Timer and scheduling utilities
- **`task/`**: Task management and execution
- **`log/`**: Logging configuration
- **`codec/`**: Audio/video codec implementations
- **`sdp/`**: Session Description Protocol handling

### External Integrations
- **`googleapis/`**: Google Cloud services integration
- **`yaypi/`**: External API integrations
- **`spandsp-sys/`**: DSP library bindings
- **`opus-sys/`**: Opus codec bindings
- **`lame/`**: MP3 encoding support
--------------------------------------------------------------------------------------
## Configuration

### Configuration Files
- **`/etc/nebula/nebula.conf`**: Main configuration file
- **Environment variables**: Override configuration values
- **Database configuration**: Connection strings and settings

### Key Configuration Sections
- **Network settings**: IP addresses, ports, protocols
- **Database settings**: Connection strings, pool settings
- **Redis settings**: Cache configuration, connection details
- **Media settings**: Codec preferences, quality settings
- **Logging settings**: Log levels, output destinations

## Deployment Architecture

### Single Node Deployment
```
┌─────────────────────────────────────┐
│            Single Server            │
│  ┌─────┐ ┌─────┐ ┌─────┐ ┌─────┐  │
│  │Proxy│ │Switch│ │Media│ │ DB  │  │
│  └─────┘ └─────┘ └─────┘ └─────┘  │
│  ┌─────┐ ┌─────┐                   │
│  │Redis│ │Logs │                   │
│  └─────┘ └─────┘                   │
└─────────────────────────────────────┘
```

### Distributed Deployment
```
┌─────────────┐  ┌─────────────┐  ┌─────────────┐
│   Proxy     │  │ Switchboard │  │    Media    │
│   Server    │  │   Server    │  │   Server    │
└─────────────┘  └─────────────┘  └─────────────┘
       │                │                │
       └────────────────┼────────────────┘
                        │
                ┌─────────────┐
                │   Shared    │
                │  Services   │
                │ (DB/Redis)  │
                └─────────────┘
```

## Performance Characteristics

### Scalability
- **Horizontal scaling**: Multiple instances of each component
- **Load balancing**: Proxy distributes load across switchboard instances
- **Media scaling**: Multiple media servers for high concurrent calls
- **Database scaling**: Read replicas and connection pooling

### Performance Metrics
- **Call capacity**: 1000+ concurrent calls per media server
- **Latency**: <50ms end-to-end for local calls
- **Throughput**: 1000+ SIP messages per second
- **Memory usage**: ~100MB per 100 concurrent calls

## Security Considerations

### Authentication
- **SIP authentication**: Digest authentication support
- **API authentication**: JWT token-based authentication
- **Database security**: Encrypted connections and access controls

### Network Security
- **TLS support**: Encrypted SIP and RPC communication
- **Firewall configuration**: Port access controls
- **Rate limiting**: Protection against DoS attacks

## Monitoring and Observability

### Logging
- **Structured logging**: JSON-formatted logs with tracing
- **Log levels**: Debug, Info, Warn, Error
- **Log rotation**: Automatic log file management

### Metrics
- **Call metrics**: Active calls, call duration, success rates
- **Performance metrics**: CPU usage, memory usage, network I/O
- **Error metrics**: Failed calls, timeouts, errors

### Health Checks
- **Service health**: HTTP health check endpoints
- **Database health**: Connection and query health
- **Redis health**: Cache and queue health

## Development and Testing

### Development Setup
- **Rust toolchain**: Latest stable Rust
- **Dependencies**: PostgreSQL, Redis, FFmpeg
- **Build system**: Cargo workspace with multiple crates

### Testing Strategy
- **Unit tests**: Individual component testing
- **Integration tests**: Inter-component communication
- **Load testing**: Performance and scalability testing
- **SIP testing**: Protocol compliance testing

## Future Enhancements

### Planned Features
- **WebRTC support**: Browser-based calling
- **Video conferencing**: Multi-party video calls
- **AI integration**: Voice recognition and transcription
- **Cloud deployment**: Kubernetes and container support

### Performance Improvements
- **Zero-copy networking**: Reduce memory allocations
- **SIMD optimizations**: Accelerate media processing
- **Connection pooling**: Optimize database connections
- **Caching improvements**: Enhanced Redis usage
