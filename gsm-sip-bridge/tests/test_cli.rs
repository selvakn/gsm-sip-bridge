use clap::Parser;
use gsm_sip_bridge::cli::Cli;

#[test]
fn test_parse_full_args() {
    let args = vec![
        "gsm-sip-bridge",
        "--config",
        "/etc/config.toml",
        "--verbose",
        "--serial",
        "/dev/ttyUSB3",
        "--audio",
        "hw:2,0",
    ];

    let cli = Cli::try_parse_from(args).unwrap();
    assert_eq!(cli.config.to_str().unwrap(), "/etc/config.toml");
    assert!(cli.verbose);
    assert_eq!(cli.serial.unwrap().to_str().unwrap(), "/dev/ttyUSB3");
    assert_eq!(cli.audio.unwrap(), "hw:2,0");
}

#[test]
fn test_config_required() {
    let args = vec!["gsm-sip-bridge"];
    let result = Cli::try_parse_from(args);
    assert!(result.is_err());
}

#[test]
fn test_serial_requires_audio() {
    let args = vec![
        "gsm-sip-bridge",
        "--config",
        "config.toml",
        "--serial",
        "/dev/ttyUSB3",
    ];
    let result = Cli::try_parse_from(args);
    assert!(result.is_err());
}

#[test]
fn test_audio_requires_serial() {
    let args = vec![
        "gsm-sip-bridge",
        "--config",
        "config.toml",
        "--audio",
        "hw:2,0",
    ];
    let result = Cli::try_parse_from(args);
    assert!(result.is_err());
}

#[test]
fn test_unknown_flag_fails() {
    let args = vec!["gsm-sip-bridge", "--config", "c.toml", "--badarg"];
    let result = Cli::try_parse_from(args);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.exit_code(), 2);
}
