# Nebula Data Flow Documentation

## Overview

This document explains how data flows through the Nebula VoIP system. Understanding these flows is crucial for debugging, optimization, and extending the system.

## Call Establishment Flow

### Basic Call Setup (SIP INVITE)

```
Client A                    Proxy                Switchboard              Media
   |                         |                       |                     |
   |-- INVITE -------------->|                       |                     |
   |                         |-- RPC: NewInvite ---->|                     |
   |                         |                       |-- RPC: CreateStream->|
   |                         |                       |                     |-- Setup RTP
   |                         |                       |                     |
   |                         |<-- RPC: Response -----|                     |
   |                         |                       |                     |
   |<-- 100 Trying ----------|                       |                     |
   |                         |                       |                     |
   |                         |-- RPC: Ringing ------>|                     |
   |                         |                       |                     |
   |<-- 180 Ringing ---------|                       |                     |
   |                         |                       |                     |
   |                         |-- RPC: Answer ------->|                     |
   |                         |                       |                     |
   |<-- 200 OK --------------|                       |                     |
   |                         |                       |                     |
   |-- ACK ----------------->|                       |                     |
   |                         |                       |                     |
   |                         |                       |                     |
   |======================= RTP Media Stream ==============================|
```

### What Happens at Each Step

1. **Client sends INVITE**: User initiates a call
2. **Proxy receives INVITE**: Validates and routes the request
3. **Switchboard processes**: Determines call routing and creates channel
4. **Media server setup**: Prepares RTP streams for audio/video
5. **Response flow**: Status updates flow back to client
6. **Media starts**: RTP packets flow between endpoints

## SIP Message Routing

### Message Flow Through Proxy

```
Incoming SIP Message
        |
        v
   [Validate] --> Invalid? --> Send 400 Bad Request
        |
        v
   [Authenticate] --> Failed? --> Send 401 Unauthorized
        |
        v
   [Lookup User] --> Not Found? --> Send 404 Not Found
        |
        v
   [Route Decision] --> Local? --> Switchboard
                      --> Remote? --> External Proxy
        |
        v
   [Forward Message] --> Add Via Header
        |
        v
   [Track Transaction] --> Store in Redis
        |
        v
   [Send Response] --> Client
```

### Key Routing Decisions

- **Local calls**: Route to switchboard for processing
- **External calls**: Forward to external SIP provider
- **Registration**: Handle user location updates
- **Presence**: Update user availability status

## Media Stream Flow

### RTP Packet Processing

```
RTP Packet Received
        |
        v
   [Validate Header] --> Invalid? --> Drop Packet
        |
        v
   [Check SSRC] --> Unknown? --> Create New Stream
        |
        v
   [Decode Audio] --> Opus/G.722/PCM
        |
        v
   [Apply Effects] --> Noise Suppression, Echo Cancellation
        |
        v
   [Mix Audio] --> Conference Mixing
        |
        v
   [Encode Output] --> Target Codec
        |
        v
   [Send RTP] --> Destination
```

### Media Processing Pipeline

1. **Packet Reception**: Receive RTP packets from network
2. **Stream Management**: Track multiple audio/video streams
3. **Codec Processing**: Decode incoming, encode outgoing
4. **Audio Processing**: Apply effects and mixing
5. **Packet Transmission**: Send processed packets

## Error Handling Flow

### SIP Error Processing

```
SIP Error Received
        |
        v
   [Log Error] --> Debug Information
        |
        v
   [Determine Type] --> Client Error? --> Send 4xx
                      --> Server Error? --> Send 5xx
                      --> Network Error? --> Retry
        |
        v
   [Update State] --> Transaction State Machine
        |
        v
   [Cleanup] --> Release Resources
        |
        v
   [Notify Components] --> RPC Error Message
```

### Common Error Scenarios

- **Network timeouts**: Retry with exponential backoff
- **Authentication failures**: Log and send appropriate response
- **Resource exhaustion**: Graceful degradation
- **Protocol errors**: Log and terminate gracefully

## Inter-Service Communication

### RPC Message Flow

```
Service A                    Redis Stream              Service B
   |                             |                       |
   |-- RPC Message ------------->|                       |
   |                             |-- Store Message ----->|
   |                             |                       |
   |                             |<-- Consume Message ---|
   |                             |                       |
   |                             |<-- Process Message ---|
   |                             |                       |
   |                             |-- Send Response ----->|
   |                             |                       |
   |<-- RPC Response ------------|                       |
```

### RPC Message Types

- **Call Control**: Setup, modify, terminate calls
- **Media Control**: Start, stop, modify media streams
- **User Management**: Registration, presence updates
- **System Events**: Health checks, configuration updates

## Database Operations Flow

### User Location Updates

```
SIP REGISTER Received
        |
        v
   [Validate Request] --> Invalid? --> Send 400 Bad Request
        |
        v
   [Check Credentials] --> Failed? --> Send 401 Unauthorized
        |
        v
   [Update Location] --> Database Transaction
        |
        v
   [Cache Update] --> Redis Cache
        |
        v
   [Send Response] --> 200 OK
        |
        v
   [Log Event] --> CDR Database
```

### Database Transaction Flow

1. **Begin Transaction**: Start database transaction
2. **Validate Data**: Check data integrity
3. **Update Records**: Modify user location
4. **Update Cache**: Sync Redis cache
5. **Commit Transaction**: Make changes permanent
6. **Log Event**: Record in CDR system

## Performance Considerations

### Bottlenecks to Watch

- **SIP Message Processing**: High message rates can overwhelm proxy
- **Media Processing**: CPU-intensive audio/video processing
- **Database Queries**: Slow queries can delay call setup
- **Network Latency**: High latency affects call quality

### Optimization Strategies

- **Connection Pooling**: Reuse database connections
- **Caching**: Cache frequently accessed data
- **Load Balancing**: Distribute load across instances
- **Async Processing**: Non-blocking operations

## Debugging Data Flow Issues

### Common Problems

1. **Call Setup Failures**: Check SIP message routing
2. **Media Quality Issues**: Monitor RTP packet loss
3. **Performance Problems**: Profile database queries
4. **Authentication Failures**: Verify user credentials

### Debugging Tools

- **SIP Tracing**: Enable detailed SIP message logging
- **Media Monitoring**: Monitor RTP stream quality
- **Database Profiling**: Analyze query performance
- **System Metrics**: Monitor CPU, memory, network usage

## Monitoring and Observability

### Key Metrics to Monitor

- **Call Success Rate**: Percentage of successful calls
- **Call Setup Time**: Time from INVITE to 200 OK
- **Media Quality**: RTP packet loss, jitter, latency
- **System Performance**: CPU, memory, network usage

### Alerting Thresholds

- **Call Success Rate**: < 95%
- **Call Setup Time**: > 5 seconds
- **Packet Loss**: > 1%
- **CPU Usage**: > 80%

## Conclusion

Understanding data flow is essential for maintaining and optimizing the Nebula system. These flows show how different components work together to provide reliable VoIP services. When debugging issues, trace the data flow to identify where problems occur and implement appropriate fixes.
