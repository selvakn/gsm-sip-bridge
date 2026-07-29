//! `ims-register` / `ims-call` — standalone IMS-AKA diagnostics that do not
//! start the daemon or touch the `CardPool`.

use std::process::ExitCode;

fn build_ims_register_config(args: &crate::cli::ImsRegisterArgs) -> crate::ims::ImsRegisterConfig {
    crate::ims::ImsRegisterConfig {
        modem_port: args.modem.clone(),
        pcsc_reader: false,
        pcscf_addr: args.pcscf,
        pcscf_port: args.pcscf_port,
        mcc: args.mcc.clone(),
        mnc: args.mnc.clone(),
        imsi: args.imsi.clone(),
        imei: args.imei.clone(),
        use_tcp: args.tcp,
        sec_agree: args.sec_agree,
        msisdn: args.msisdn.clone(),
        access_network_info: crate::ims::ACCESS_NETWORK_WLAN.to_string(),
    }
}

pub(crate) fn handle_ims_register_command(args: &crate::cli::ImsRegisterArgs) -> ExitCode {
    use crate::ims::{run_register, RegisterOutcome};

    let cfg = build_ims_register_config(args);

    match run_register(&cfg) {
        Ok(RegisterOutcome::Success { status, headers }) => {
            println!("REGISTER succeeded: {status} OK");
            for (k, v) in headers {
                println!("  {k}: {v}");
            }
            ExitCode::SUCCESS
        }
        Ok(RegisterOutcome::Rejected { status, reason }) => {
            eprintln!("REGISTER rejected: {status} {reason}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) fn handle_ims_call_command(args: &crate::cli::ImsCallArgs) -> ExitCode {
    use crate::ims::call::{run_call, CallConfig, CallOutcome};
    use std::time::Duration;

    let cfg = CallConfig {
        register: build_ims_register_config(&args.register),
        callee: args.to.clone(),
        record_path: args.record.clone(),
        record_sent_path: args.record_sent.clone(),
        ring_timeout: Duration::from_secs(args.ring_timeout_secs),
        call_duration: Duration::from_secs(args.call_duration_secs),
        // `ims-call` keeps sending the tone pattern, unchanged: echo is opt-in
        // so the VoWiFi diagnostic behaves exactly as it did (FR-020).
        echo: None,
        one_way_threshold_percent: crate::ims::media_stats::DEFAULT_ONE_WAY_THRESHOLD_PERCENT,
        // Historical ordering, so the VoWiFi diagnostic's offer is unchanged
        // (FR-020). Carriers on that path require wideband anyway.
        codec_offer: crate::ims::sdp::CodecOffer::legacy(amr_safe::is_available()),
    };

    match run_call(&cfg) {
        Ok(CallOutcome::Answered {
            recorded_path,
            recorded_samples,
            sent_path,
            sent_samples,
            ..
        }) => {
            println!(
                "call answered — recorded {recorded_samples} received samples to {}",
                recorded_path.display()
            );
            if let Some(sent_path) = sent_path {
                println!(
                    "  and {sent_samples} sent samples to {}",
                    sent_path.display()
                );
            }
            ExitCode::SUCCESS
        }
        Ok(CallOutcome::NotAnswered { status, reason }) => {
            eprintln!("call not answered: {status} {reason}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
