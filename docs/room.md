## Room REST API

The endpoint is the very old Aura one.

Create

```
POST /v1/rooms

{
    "room": {
        "name": "test room 123",
        "password": "password",                                         # Optional
        "open": false,
        "force_password": false,
        "force_waiting_room": false,
        "members": [
            {
                "user_id": "02ba016d-b3c2-47d2-a0e2-e814f9daa822",
                "admin": true
            },
            {
                "user_id": "5dc9dc1a-db23-4695-a08f-25c8167fff31",
                "admin": false
            }
        ]
    }
}
```

Update

```
PUT /v1/rooms/room_uuid

{
    "room": {
        "name": "test room 123",
        "password": "password",                                         # Optional
        "open": false,
        "force_password": false,
        "force_waiting_room": false,
        "members": [
            {
                "user_id": "02ba016d-b3c2-47d2-a0e2-e814f9daa822",
                "admin": true
            },
            {
                "user_id": "5dc9dc1a-db23-4695-a08f-25c8167fff31",
                "admin": false
            }
        ]
    }
}
```

Get All

```
GET /v1/rooms
```

Get One

```
GET /v1/rooms/room_uuid
```

Delete

```
DELETE /v1/rooms/room_uuid
```

The following APIs' endpoint is http://10.154.0.11:8122

Get all members in a room

```
GET /v1/rooms/room_uuid/members
```

## Create Temp Room

A sip user can start a on demand confernece by creating a temp room

```
{
    "id": "request_id",
    "method": "create_temp_room",
    "params": {
        "call_id": "call uuid",            # the uuid of this call session
        "username": "username",            # for desktop app
        "password": "password",            # for desktop app
        "device": "device_name",           # for mobile app
        "device_token": "device_token",    # for mobile app
        "sdp": "sdp text",                 # the sdp body for peerconnection
        "invited": ["1001", "1002", "07555666888"] # a list of invited extension number of external number
        "audio_muted": false,              # if audio is muted when joining
        "video_muted": false,              # if video is muted when joining
    }
}
```

The invited extensions will receive a sip call, and they will join the room after answered.

To be able to join the room with video, the APP needed to register as a video registration. And the registration needs to be refreshed every 1 minute.

```
{
    "id": "request_id",
    "method": "register_room_api",
    "params": {
        "username": "username",            # for desktop app
        "password": "password",            # for desktop app
        "device": "device_name",           # for mobile app
        "device_token": "device_token",    # for mobile app
    }
}
```

The video registered clients will receive this notification for a invite to the room

```
{
    "method": "room_invite",
    "params": {
        "room": "room_id",                 # the uuid of the room
        "call_id": "call_id",              # the uuid of this call session
        "from_user": "from sip user uuid", # the uuid of the sip user that this invites is from
        "auth_token": "auth_token",        # the auth token that you can use to join the room
    }
}
```

If the users is registered at multiple places, and get answered by one registration, the other video clients will receive this

```
{
    "method": "room_invite_completed_elsewhere",
    "params": {
        "room": "room_id",                 # the uuid of the room
    }
}
```

Then you can use the ```auth_token``` to join the room

If the user rejected the invitation, the sender will receive this notification

```
{
    "method": "room_invite_reject",
    "params": {
        "room": "room_id",                 # the uuid of the room
        "call_id": call_id,                # the uuid of the call id
    }
}
```

## Convert a sip call to room

A sip user can convert a sip call to a conference room

```
{
    "id": "request_id",
    "method": "convert_to_room",
    "params": {
        "call_id": "call id",            # the call id
        "invited": ["1001", "1002", "07555666888"] # a list of invited extension number of external number
    }
}
```

## Watch the room

If a sip call was converted to room, the leg b of the call will receive a auth token.
And it can use that auth token to watch the room to get the room updates etc.

```
{
    "id": "request_id",
    "method": "watch_room",
    "params": {
        "auth_token": "auth token"            # the auth token
    }
}
```

## Invite a extension to room

A room admin can invite people to the room by extension number

```
{
    "id": "request_id",
    "method": "invite_exten",
    "params": {
        "extens": ["1001", "1002", "07555666888"] # a list of invited extension number of external number
    }
}
```

## Authentication

After connecting to the websocket, do an authenticate request first with

```
{
    "id": "request_id",
    "method": "authenticate",
    "params": {
         "room": "room_uuid",               # the uuid for the room you want to join
         "call_id": "call uuid",            # the uuid of this call session
         "username": "username",            # for desktop app
         "password": "password",            # for desktop app
         "device": "device_name",           # for mobile app
         "device_token": "device_token"     # for mobile app
         "display_name": "Test 123",        # the display name you want to show in the room
         "room_password": "password",       # for room password authentication
    }
}
```

The server will respond with the auth token for this call session
```
{
    "id": "request_id",
    "result": {
        "auth_token": "auth token of the call session"
    }
}
```

#### After you're autenticated, you can query the members in the room before joining

```
{
    "id": "request_id",
    "method": "get_room_members",
    "params": {}
}
```

#### If you're admin, you can aslo query the members in the waiting room

```
{
    "id": "request_id",
    "method": "get_waiting_room_members",
    "params": {}
}
```

## Anonymous and waiting room

To join room as anonymous

```
{
    "id": "request_id",
    "method": "authenticate",
    "params": {
         "room": "room_uuid",               # the uuid for the room you want to join
         "call_id": "call_uuid",            # the uuid of this call session
         "display_name": "Test 123",        # the display name you want to show in the room
    }
}
```

You'll receive 403 error

```
{
    "id": "request_id",
    "error": {
        "code":403,
        "message": "not allowed for this room, waiting to be accpeted"
    }
}
```

The members in the room will receive this notification

```
{
    "method": "member_waiting",
    "params": {
        "member": "call_uuid",
        "name": "Test 123",
        "user": "sip user uuid"             # only set if the member was a sip user that's not invited
    }
}
```

#### If you'd like to nudge the admins

```
{
    "method": "still_waiting",
    "params": {}
}
```

#### To allow the member to the room, you can send this request

```
{
    "id": "request_id",
    "method":"allow_member",
    "params":{
        "member": "call_uuid",
        "auth_token": "auth_token"
    }
}
```

If success the member will receive the auth token to join the room

```
{
    "method": "member_allowed",
    "params":{
        "auth_token":"auth_token"
    }
}
```

#### To reject the waiting room member to the room, you can send this request

```
{
    "id": "request_id",
    "method": "reject_member",
    "params": {
        "member": "call_uuid",
        "auth_token": "auth_token"
    }
}
```

The waiting room member will receive a rejected notificatoin

```
{
    "method": "member_rejected",
    "params": {}
}
```

#### When the member in the waiting room left the room, the members in the room will receive

```
{
    "method": "member_leave_waiting_room",
    "params": {
        "member": "member_id",
    }
}
```

## Join Room

To join the room

```
{
    "id": "request_id",
    "method": "join_room",
    "params": {
        "sdp": "sdp text",
        "auth_token": "auth token",
        "audio_muted": false,
        "video_muted": false,
    }
}
```

## Subscribe Video Feed

To subsribe the video feeds of the members

```
{
    "id": "request_id",
    "method": "subscribe_room",
    "params": {
        "sdp": "sdp text",
        "members": [
            {
                "id": "member1_uuid",
                "video_mid": "mid of the transceiver",
                "width": 640,
                "height": 320,
            },
            {
                "id": "member2_uuid",
                "video_mid": "mid of the transceiver",
                "width": 640,
                "height": 320,
            }
        ],
        "auth_token": "auth token"
    }
}
```

## Leave Room

```
{
    "method": "leave_room",
    "params": {}
}
```

## Raise/Unraise Hand

```
{
    "method": "raise_hand",
    "params": {}
}

{
    "method": "unraise_hand",
    "params": {}
}
```

## Start/Stop Screen Share

```
{
    "method": "start_screen_share",
    "params": {}
}

{
    "method": "stop_screen_share",
    "params": {}
}
```

## Send Reaction

```
{
    "method": "send_reaction",
    "params": {
        "reaction": "thumbs_up"
    }
}
```

## Mute/Unmute

Notifications for mute/unumte audio/video for sending to the server.

```
{
    "method": "mute_audio",
    "params": {}
}

{
    "method": "unmute_audio",
    "params": {}
}

{
    "method": "mute_video",
    "params": {}
}

{
    "method": "unmute_video",
    "params": {}
}
```

Notifications from the server to clients

```
{
    "method": "member_mute_audio",
    "params": {
        "member": "member_id",
        "by_admin": false,
    }
}

{
    "method": "member_unmute_audio",
    "params": {
        "member": "member_id",
        "by_admin": false,
    }
}

{
    "method": "member_mute_video",
    "params": {
        "member": "member_id",
        "by_admin": false,
    }
}

{
    "method": "member_unmute_video",
    "params": {
        "member": "member_id",
        "by_admin": false,
    }
}
```

## Admin remove member from room

```
{
    "id": "request_id",
    "method": "remove_member",
    "params": {
        "member": "member_id",
    }
}
```

## Admin Mute/Unmute

Requests from admins to mute/unmute audio/video other members in the room

```
{
    "id": "request_id",
    "method": "mute_member_audio",
    "params": {
        "member": "member_id",
    }
}

{
    "id": "request_id",
    "method": "unmute_member_audio",
    "params": {
        "member": "member_id",
    }
}

{
    "id": "request_id",
    "method": "mute_member_video",
    "params": {
        "member": "member_id",
    }
}

{
    "id": "request_id",
    "method": "unmute_member_video",
    "params": {
        "member": "member_id",
    }
}
```
