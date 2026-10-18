// Compile-time settings. Copy to remote.local.rs to customize privately.
// Entries correspond to GPIO0, GPIO1, and GPIO10, in this order.
pub const TARGET_NAME: &str = "Sonos Küche";
// Sonos modelName without the optional "Sonos " prefix.
pub const TARGET_MODEL: &str = "One SL";
pub const STATIONS: [Station; 3] = [
    Station {
        name: "Deutschlandfunk",
        stream_url: "https://st01.sslstream.dlf.de/dlf/01/128/mp3/stream.mp3",
    },
    Station {
        name: "radioeins (rbb)",
        stream_url: "x-rincon-mp3radio://dispatcher.rndfnk.com/rbb/radioeins/live/mp3/mid",
    },
    Station {
        name: "Deutschlandfunk Kultur",
        stream_url: "https://st02.sslstream.dlf.de/dlf/02/128/mp3/stream.mp3",
    },
];
