use traccia::Color;
use traccia::Colored;
use traccia::Level;
use traccia::LevelFilter;

struct Formatter;

impl traccia::Formatter for Formatter {
    fn format(&self, record: &traccia::Record) -> String {
        let (name, color) = match record.level() {
            Level::Trace => ("trace", Color::Cyan),
            Level::Debug => ("debug", Color::Magenta),
            Level::Info => ("info", Color::Green),
            Level::Warn => ("warn", Color::Yellow),
            Level::Error => ("error", Color::Red),
        };

        format!("{}: {}", name.foreground_color(color), record.args())
    }
}

fn min_level() -> LevelFilter {
    if cfg!(debug_assertions) {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    }
}

pub fn init() {
    let _ = traccia::init(
        traccia::Config::default()
            .with_min_level(min_level())
            .with_formatter(Formatter),
    );
}
