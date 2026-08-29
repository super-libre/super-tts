// SPDX-License-Identifier: GPL-3.0-only
use clap::{Arg, Command, ValueEnum};
use super_tts_cosmic_applet::{VERSION, VisualizationSide};

#[derive(ValueEnum, Clone, Debug)]
enum Side {
    Full,
    Left,
    Right,
}

impl From<Side> for VisualizationSide {
    fn from(side: Side) -> Self {
        match side {
            Side::Full => VisualizationSide::Full,
            Side::Left => VisualizationSide::Left,
            Side::Right => VisualizationSide::Right,
        }
    }
}

fn main() -> cosmic::iced::Result {
    // `Info` default (the bare `env_logger::init()` here defaulted to `error`,
    // making the applet effectively silent vs its siblings) (Tier 2 #6).
    super_tts_shared::logging::init();
    log::info!("Starting Super TTS applet with version {VERSION}");

    let matches = Command::new("super-tts-cosmic-applet")
        .version(VERSION)
        .about("COSMIC panel applet for Super TTS text-to-speech service")
        .arg(
            Arg::new("side")
                .long("side")
                .short('s')
                .help("Visualization side to display")
                .value_parser(clap::value_parser!(Side))
                .default_value("full"),
        )
        .get_matches();

    let side = matches.get_one::<Side>("side").unwrap().clone();
    let visualization_side = VisualizationSide::from(side);

    cosmic::applet::run::<super_tts_cosmic_applet::SuperTtsApplet>(visualization_side)
}
