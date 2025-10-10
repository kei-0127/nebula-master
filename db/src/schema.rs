table! {
    sip_users (id) {
        id -> BigInt,
        uuid -> Text,
        name -> Text,
        domain -> Nullable<Text>,
        tenant_id -> Text,
        extension -> Nullable<BigInt>,
        name_lower -> Text,
        deleted -> BigInt,
        display_name -> Nullable<Text>,
        caller_id -> Nullable<Text>,
        password -> Text,
        music_on_hold -> Nullable<Text>,
        recording -> Nullable<Bool>,
        unlimited_recording -> Nullable<Bool>,
        require_auth -> Bool,
        duration -> Nullable<BigInt>,
        transfer_mailbox -> Bool,
        transfer_show_sipuser -> Bool,
        transfer_backup_extension -> Nullable<BigInt>,
        available -> Bool,
        trunk -> Bool,
        pickup -> Nullable<Bool>,
        be_picked_up -> Nullable<Bool>,
        listen -> Nullable<Bool>,
        whisper -> Nullable<Bool>,
        barge -> Nullable<Bool>,
        login -> Nullable<Bool>,
        be_listened -> Nullable<Bool>,
        be_whispered -> Nullable<Bool>,
        be_barged -> Nullable<Bool>,
        be_login -> Nullable<Bool>,
        encryption -> Nullable<Bool>,
        video -> Nullable<Bool>,
        cc -> Nullable<Text>,
        personal_vmbox -> Nullable<Text>,
        device -> Nullable<Text>,
        device_token -> Nullable<Text>,
        chat_device_token -> Nullable<Text>,
        mute_chat_notification-> Bool,
        force_chat_notification-> Bool,
        call_handling -> Nullable<Text>,
        timezone -> Nullable<Text>,
        msteam -> Nullable<Text>,
        msteam_domain -> Nullable<Text>,
        disable_call_waiting-> Bool,
        alert_info_patterns -> Nullable<Text>,
        pin -> Nullable<Text>,
        hot_desk_user -> Nullable<Bool>,
        aux_schedule -> Nullable<Text>,
        aux_schedule_timezone -> Nullable<Text>,
        queue_answer_wait -> Nullable<BigInt>,
        queue_no_answer_wait -> Nullable<BigInt>,
        queue_reject_wait -> Nullable<BigInt>,
        disable_external_transfer_recording -> Nullable<Bool>,
        global_dnd -> Nullable<Bool>,
        global_forward -> Nullable<Text>,
    }
}

table! {
    hunt_groups (id) {
        id -> BigInt,
        uuid -> Text,
        name -> Text,
        tenant_id -> Text,
        updated_at -> Nullable<Timestamp>,
        deleted -> BigInt,
    }
}

table! {
    hunt_group_members (id) {
        id -> BigInt,
        sip_user_uuid -> Text,
        hunt_group_uuid -> Text,
        deleted -> BigInt,
    }
}

table! {
    extensions (id) {
        id -> BigInt,
        number -> BigInt,
        tenant_id -> Text,
        deleted -> BigInt,
        discriminator -> Text,
        parent_id -> Text,
    }
}

table! {
    numbers (id) {
        id -> BigInt,
        uuid -> Text,
        tenant_id -> Nullable<Text>,
        number -> Text,
        country_code -> Text,
        e164 -> Text,
        deleted -> BigInt,
        call_flow_id -> Nullable<Text>,
        diary_id -> Nullable<Text>,
        require_auth -> Bool,
        limit -> BigInt,
        sip_user_id -> Nullable<Text>,
        is_reseller -> Bool,
        ziron_tag -> Nullable<Text>,
        ddi_display_name_show_number -> Nullable<Bool>,
    }
}

table! {
    trunks (id) {
        id -> BigInt,
        uuid -> Text,
        tenant_id -> Text,
        deleted -> BigInt,
        name -> Text,
        user -> Text,
        domain -> Text,
        port -> Nullable<BigInt>,
        transport -> Text,
        cc -> Nullable<Text>,
        caller_id -> Nullable<Text>,
        show_pai -> Nullable<Bool>,
        show_diversion -> Nullable<Bool>,
        pass_pai -> Nullable<Bool>,
        codecs -> Nullable<Text>,
        user_agent -> Nullable<Bool>,
        display_name_as_callerid -> Nullable<Bool>,
    }
}

table! {
    trunk_auth_ips (id) {
        id -> BigInt,
        uuid -> Text,
        tenant_id -> Text,
        deleted -> BigInt,
        ip -> Text,
        info -> Text,
    }
}

table! {
    trunk_auths (id) {
        id -> BigInt,
        uuid -> Text,
        tenant_id -> Text,
        deleted -> BigInt,
        trunk_uuid -> Text,
        discriminator -> Text,
        member_uuid -> Text,
    }
}

table! {
    recording (id) {
        id -> BigInt,
        tenant_id -> Text,
        deleted -> BigInt,
        expires_in -> BigInt,
        global_enabled -> Nullable<Bool>,
    }
}

table! {
    call_flows (id) {
        id -> BigInt,
        uuid -> Text,
        tenant_id -> Text,
        deleted -> BigInt,
        name -> Text,
        flow -> Text,
        unlimited_recording -> Bool,
        shown_in_callerid -> Bool,
        show_original_callerid -> Bool,
        custom_callerid -> Nullable<Text>,
    }
}

table! {
    shortcodes (id) {
        id -> BigInt,
        uuid -> Text,
        tenant_id -> Text,
        deleted -> BigInt,
        shortcode -> Text,
        in_channel -> Bool,
        out_channel -> Bool,
        feature -> Text,
        no_star -> Nullable<Bool>,
    }
}

table! {
    diaries (uuid) {
        uuid -> Uuid,
        tenant_id -> Text,
        deleted -> Text,
        name -> Text,
        records -> Text,
        timezone -> Text,
    }
}

table! {
    sipuser_permissions (uuid) {
        uuid -> Uuid,
        deleted -> Nullable<Text>,
        tenant_id -> Nullable<Text>,
        from_user -> Nullable<Text>,
        to_user -> Nullable<Text>,
        from_group -> Nullable<Text>,
        to_group -> Nullable<Text>,
        feature -> Text,
    }
}

table! {
    sipuser_time_schedules (uuid) {
        uuid -> Uuid,
        next_run -> Nullable<Timestamptz>,
        last_run -> Nullable<Timestamptz>,
    }
}

table! {
    login_as_users (uuid) {
        created_at -> Nullable<Timestamptz>,
        deleted_at -> Nullable<Timestamptz>,
        uuid -> Uuid,
        deleted -> Text,
        user -> Text,
        login_as -> Text,
    }
}

table! {
    tenant_extensions (uuid) {
        uuid -> Uuid,
        created_at -> Timestamptz,
        deleted_at -> Nullable<Timestamptz>,
        tenant_id -> Text,
        deleted -> Text,
        number -> BigInt,
        dst_tenant_id -> Text,
    }
}

table! {
    speed_dials (uuid) {
        uuid -> Uuid,
        deleted -> Text,
        tenant_id -> Text,
        code -> Text,
        number -> Text,
    }
}

table! {
    mobile_apps (uuid) {
        uuid -> Uuid,
        created_at -> Nullable<Timestamptz>,
        deleted -> Text,
        user -> Text,
        device -> Text,
        device_token -> Text,
        chat_device_token -> Text,
        dnd -> Bool,
        dnd_allow_internal -> Bool,
        mute_chat_notification -> Bool,
        force_chat_notification -> Bool,
    }
}

table! {
    queue_groups (uuid) {
        uuid -> Text,
        tenant_id -> Text,
        deleted -> BigInt,
        name -> Text,
        strategy -> Text,
        members -> Text,
        ring_progressively -> Bool,
        ring_timeout -> BigInt,
        duration -> BigInt,
        answer_wait -> BigInt,
        no_answer_wait -> BigInt,
        reject_wait -> BigInt,
        hide_missed_calls -> Nullable<Bool>,
    }
}

table! {
    queue_hints (uuid) {
        uuid -> Text,
        tenant_id -> Text,
        deleted -> BigInt,
        name -> Text,
        frequency -> BigInt,
        audios -> Text,
    }
}

table! {
    moh_classes (uuid) {
        uuid -> Text,
        tenant_id -> Text,
        deleted -> BigInt,
        name -> Text,
        random -> Bool,
    }
}

table! {
    moh_music (id) {
        id -> BigInt,
        deleted -> BigInt,
        sound_uuid -> Text,
        moh_class_uuid -> Text,
    }
}

table! {
    voicemail_users (id) {
        id -> BigInt,
        uuid -> Text,
        tenant_id -> Text,
        deleted -> BigInt,
        name -> Text,
        greeting -> Text,
        beep -> Bool,
        password -> Text,
        mailbox -> Text,
        play_timestamp -> Bool,
        play_callerid -> Bool,
        duration_limit -> Nullable<BigInt>,
    }
}

table! {
    voicemail_messages (id) {
        id -> BigInt,
        uuid -> Text,
        deleted -> BigInt,
        deleted_at -> Nullable<Timestamp>,
        voicemail_user_uuid -> Text,
        folder -> Text,
        callerid -> Text,
        calleeid -> Text,
        time -> Timestamp,
        duration -> BigInt,
    }
}

table! {
    voicemail_menus (id) {
        id -> BigInt,
        uuid -> Text,
        deleted -> BigInt,
        tenant_id -> Text,
        extension -> BigInt,
        mailbox -> Text,
        name -> Text,
    }
}

table! {
    sounds (id) {
        id -> BigInt,
        uuid -> Text,
        tenant_id -> Text,
        deleted -> BigInt,
        name -> Text,
        time -> Nullable<Timestamptz>,
        duration -> Double,
    }
}

table! {
    voicemail_members (id) {
        id -> BigInt,
        deleted -> BigInt,
        voicemail_user_uuid -> Text,
        discriminator -> Text,
        member_uuid -> Text,
    }
}

table! {
    providers (id) {
        id -> BigInt,
        name -> Text,
        host -> Nullable<Text>,
        user -> Nullable<Text>,
        password -> Nullable<Text>,
        uri -> Nullable<Text>,
        location -> Nullable<Text>,
        realm -> Nullable<Text>,
    }
}

table! {
    cdr_items (uuid) {
        uuid -> Uuid,
        tenant_id -> Text,
        start -> Timestamptz,
        answer -> Nullable<Timestamptz>,
        end -> Nullable<Timestamptz>,
        disposition -> Nullable<Text>,
        duration -> Nullable<BigInt>,
        billsec -> Nullable<BigInt>,
        talking_start -> Nullable<Timestamptz>,
        talking_end -> Nullable<Timestamptz>,
        talking_duration -> Nullable<BigInt>,
        call_type -> Nullable<Text>,
        from_uuid -> Nullable<Text>,
        from_exten -> Nullable<Text>,
        from_type -> Nullable<Text>,
        from_user -> Nullable<Text>,
        from_name -> Nullable<Text>,
        to_uuid -> Nullable<Text>,
        to_exten -> Nullable<Text>,
        to_type -> Nullable<Text>,
        to_user -> Nullable<Text>,
        to_name -> Nullable<Text>,
        answer_uuid -> Nullable<Text>,
        answer_exten -> Nullable<Text>,
        answer_type -> Nullable<Text>,
        answer_user -> Nullable<Text>,
        answer_name -> Nullable<Text>,
        callflow_uuid -> Nullable<Text>,
        callflow_name -> Nullable<Text>,
        parent -> Nullable<Text>,
        child -> Nullable<Text>,
        recording -> Nullable<Text>,
        cost -> Nullable<Numeric>,
        parent_cost -> Nullable<Numeric>,
        queue_children -> Nullable<Text>,
        ziron_tag -> Nullable<Text>,
        hangup_direction -> Nullable<Text>,
        recording_uploaded -> Nullable<Bool>,
        caller_id -> Nullable<Text>,
        caller_id_name -> Nullable<Text>,
        operator_code -> Nullable<Text>,
    }
}

table! {
    cdr_events (uuid) {
        uuid -> Uuid,
        cdr_id -> Uuid,
        event_type -> Text,
        start -> Timestamptz,
        answer -> Nullable<Timestamptz>,
        end -> Nullable<Timestamptz>,
        duration -> Nullable<BigInt>,
        billsec -> Nullable<BigInt>,
        user_id -> Nullable<Text>,
        bridge_id -> Nullable<Text>,
        user_agent -> Nullable<Text>,
        to_type -> Nullable<Text>,
        hangup_direction -> Nullable<Text>,
        caller_id -> Nullable<Text>,
        caller_id_name -> Nullable<Text>,
    }
}

table! {
    rooms (uuid) {
        uuid -> Uuid,
        deleted -> Text,
        name -> Text,
        tenant_id -> Text,
        password -> Nullable<Text>,
        open -> Bool,
        force_password -> Bool,
        force_waiting_room -> Bool,
    }
}

table! {
    room_members (uuid) {
        uuid -> Uuid,
        deleted -> Text,
        user_id -> Text,
        room_id -> Text,
        admin -> Bool,
    }
}

table! {
    queue_records (uuid) {
        uuid -> Uuid,
        queue_id -> Text,
        start -> Timestamptz,
        cdr -> Nullable<Text>,
        disposition -> Nullable<Text>,
        duration -> Nullable<BigInt>,
        call_duration -> Nullable<BigInt>,
        wait_duration -> Nullable<BigInt>,
        ring_duration -> Nullable<BigInt>,
        answer_by -> Nullable<Text>,
        answer -> Nullable<Timestamptz>,
        end -> Nullable<Timestamptz>,
        ready -> Nullable<Timestamptz>,
        cdr_start -> Nullable<Timestamptz>,
        cdr_wait -> Nullable<BigInt>,
        group_id -> Nullable<Text>,
        failed_times -> Nullable<BigInt>,
        init_pos -> Nullable<BigInt>,
        hangup_pos -> Nullable<BigInt>,
    }
}

table! {
    queue_failed_users (uuid) {
        uuid -> Uuid,
        group_id -> Text,
        user_id -> Text,
        failed_times -> BigInt,
        start -> Timestamptz,
        queue_record_id -> Text,
    }
}

table! {
    black_list (id) {
        id -> BigInt,
        tenant_id -> Text,
        deleted -> BigInt,
        patterns -> Nullable<Text>,
    }
}

table! {
    faxes (id) {
        id -> BigInt,
        uuid -> Text,
        deleted -> BigInt,
        tenant_id -> Text,
        status -> Text,
        folder -> Text,
        to -> Text,
        callerid -> Text,
        time -> Timestamp,
        page -> BigInt,
    }
}

table! {
    sipuser_aux_log (id) {
        id -> BigInt,
        user_id -> Text,
        time -> Timestamptz,
        code -> Text,
        set_by -> Text,
    }
}

table! {
    sipuser_queue_login_log (id) {
        id -> BigInt,
        user_id -> Text,
        time -> Timestamptz,
        set_by -> Text,
        queue_id -> Nullable<Text>,
        login -> Bool,
    }
}
