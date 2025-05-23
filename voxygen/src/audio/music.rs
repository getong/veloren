//! Handles music playback and transitions
//!
//! Game music is controlled though a configuration file found in the source at
//! `/assets/voxygen/audio/soundtrack.ron`. Each track enabled in game has a
//! configuration corresponding to the
//! [`SoundtrackItem`](struct.SoundtrackItem.html) format, as well as the
//! corresponding `.ogg` file in the `/assets/voxygen/audio/soundtrack/`
//! directory.
//!
//! If there are errors while reading or deserialising the configuration file, a
//! warning is logged and music will be disabled.
//!
//! ## Adding new music
//!
//! To add a new item, append the details to the audio configuration file, and
//! add the audio file (in `.ogg` format) to the assets directory.
//!
//! The `length` should be provided in seconds. This allows us to know when to
//! transition to another track, without having to spend time determining track
//! length programmatically.
//!
//! An example of a new night time track:
//! ```text
//! (
//!     title: "Sleepy Song",
//!     path: "voxygen.audio.soundtrack.sleepy",
//!     length: 400.0,
//!     timing: Some(Night),
//!     biomes: [
//!         (Forest, 1),
//!         (Grassland, 2),
//!     ],
//!     site: None,
//!     activity: Explore,
//!     artist: "Elvis",
//! ),
//! ```
//!
//! Before sending an MR for your new track item:
//! - Be conscious of the file size for your new track. Assets contribute to
//!   download sizes
//! - Ensure that the track is mastered to a volume proportionate to other music
//!   tracks
//! - If you are not the author of the track, ensure that the song's licensing
//!   permits usage of the track for non-commercial use
use crate::audio::{AudioFrontend, MusicChannelTag};
use client::Client;
use common::{
    assets::{self, AssetExt, AssetHandle},
    calendar::{Calendar, CalendarEvent},
    terrain::{BiomeKind, SiteKindMeta},
    weather::WeatherKind,
};
use common_state::State;
use hashbrown::HashMap;
use kira::clock::ClockTime;
use rand::{Rng, prelude::SliceRandom, rngs::ThreadRng, thread_rng};
use serde::Deserialize;
use tracing::{debug, trace, warn};

/// Collection of all the tracks
#[derive(Debug, Deserialize)]
struct SoundtrackCollection<T> {
    /// List of tracks
    tracks: Vec<T>,
}

impl<T> Default for SoundtrackCollection<T> {
    fn default() -> Self { Self { tracks: Vec::new() } }
}

/// Configuration for a single music track in the soundtrack
#[derive(Clone, Debug, Deserialize)]
pub struct SoundtrackItem {
    /// Song title
    title: String,
    /// File path to asset
    path: String,
    /// Length of the track in seconds
    length: f32,
    loop_points: Option<(f32, f32)>,
    /// Whether this track should play during day or night
    timing: Option<DayPeriod>,
    /// Whether this track should play during a certain weather
    weather: Option<WeatherKind>,
    /// What biomes this track should play in with chance of play
    biomes: Vec<(BiomeKind, u8)>,
    /// Whether this track should play in a specific site
    sites: Vec<SiteKindMeta>,
    /// What the player is doing when the track is played (i.e. exploring,
    /// combat)
    music_state: MusicState,
    /// What activity to override the activity state with, if any (e.g. to make
    /// a long combat intro also act like the loop for the purposes of outro
    /// transitions)
    #[serde(default)]
    activity_override: Option<MusicActivity>,
    /// Song artist and website
    artist: (String, Option<String>),
}

#[derive(Clone, Debug, Deserialize)]
enum RawSoundtrackItem {
    Individual(SoundtrackItem),
    Segmented {
        title: String,
        timing: Option<DayPeriod>,
        weather: Option<WeatherKind>,
        biomes: Vec<(BiomeKind, u8)>,
        sites: Vec<SiteKindMeta>,
        segments: Vec<(String, f32, MusicState, Option<MusicActivity>)>,
        loop_points: (f32, f32),
        artist: (String, Option<String>),
    },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
enum CombatIntensity {
    Low,
    High,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
enum MusicActivity {
    Explore,
    Combat(CombatIntensity),
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
enum MusicState {
    Activity(MusicActivity),
    Transition(MusicActivity, MusicActivity),
}

/// Allows control over when a track should play based on in-game time of day
#[derive(Clone, Debug, Deserialize, PartialEq)]
enum DayPeriod {
    /// 8:00 AM to 7:30 PM
    Day,
    /// 7:31 PM to 6:59 AM
    Night,
}

/// Provides methods to control music playback
pub struct MusicMgr {
    /// Collection of all the tracks
    soundtrack: SoundtrackCollection<SoundtrackItem>,
    /// Instant at which the current track began playing
    began_playing: Option<ClockTime>,
    /// Instant at which the current track should stop
    song_end: Option<ClockTime>,
    /// Currently staying silent for gap between tracks
    is_gap: bool,
    /// Time until the next track should be played after a track ends
    gap_length: f32,
    /// Time remaining for gap
    gap_time: f64,
    /// The title of the last track played. Used to prevent a track
    /// being played twice in a row
    last_track: String,
    last_combat_track: String,
    /// Time of the last interrupt (to avoid rapid switching)
    last_interrupt_attempt: Option<ClockTime>,
    /// The previous track's activity kind, for transitions
    last_activity: MusicState,
    // For debug menu
    current_track: String,
    current_artist: String,
    track_length: f32,
    loop_points: Option<(f32, f32)>,
}

#[derive(Deserialize)]
pub struct MusicTransitionManifest {
    /// Within what radius do enemies count towards combat music?
    combat_nearby_radius: f32,
    /// Each multiple of this factor that an enemy has health counts as an extra
    /// enemy
    combat_health_factor: f32,
    /// How many nearby enemies trigger High combat music
    combat_nearby_high_thresh: u32,
    /// How many nearby enemies trigger Low combat music
    combat_nearby_low_thresh: u32,
    /// Fade in and fade out timings for transitions between channels
    pub fade_timings: HashMap<(MusicChannelTag, MusicChannelTag), (f32, f32)>,
    /// How many seconds between interrupt checks
    pub interrupt_delay: f32,
}

impl Default for MusicTransitionManifest {
    fn default() -> MusicTransitionManifest {
        MusicTransitionManifest {
            combat_nearby_radius: 40.0,
            combat_health_factor: 100.0,
            combat_nearby_high_thresh: 3,
            combat_nearby_low_thresh: 1,
            fade_timings: HashMap::new(),
            interrupt_delay: 5.0,
        }
    }
}

impl assets::Asset for MusicTransitionManifest {
    type Loader = assets::RonLoader;

    const EXTENSION: &'static str = "ron";
}

fn time_f64(clock_time: ClockTime) -> f64 { clock_time.ticks as f64 + clock_time.fraction }

impl MusicMgr {
    pub fn new(calendar: &Calendar) -> Self {
        Self {
            soundtrack: Self::load_soundtrack_items(calendar),
            began_playing: None,
            song_end: None,
            is_gap: true,
            gap_length: 0.0,
            gap_time: -1.0,
            last_track: String::from("None"),
            last_combat_track: String::from("None"),
            last_interrupt_attempt: None,
            last_activity: MusicState::Activity(MusicActivity::Explore),
            current_track: String::from("None"),
            current_artist: String::from("None"),
            track_length: 0.0,
            loop_points: None,
        }
    }

    /// Checks whether the previous track has completed. If so, sends a
    /// request to play the next (random) track
    pub fn maintain(&mut self, audio: &mut AudioFrontend, state: &State, client: &Client) {
        use common::comp::{Group, Health, Pos, group::ENEMY};
        use specs::{Join, WorldExt};

        if !audio.music_enabled() || audio.get_clock().is_none() || audio.get_clock_time().is_none()
        {
            return;
        }

        let mut activity_state = MusicActivity::Explore;

        let player = client.entity();
        let ecs = state.ecs();
        let entities = ecs.entities();
        let positions = ecs.read_component::<Pos>();
        let healths = ecs.read_component::<Health>();
        let groups = ecs.read_component::<Group>();
        let mtm = audio.mtm.read();
        let mut rng = thread_rng();

        if audio.combat_music_enabled {
            if let Some(player_pos) = positions.get(player) {
                // TODO: `group::ENEMY` will eventually be moved server-side with an
                // alignment/faction rework, so this will need an alternative way to measure
                // "in-combat-ness"
                let num_nearby_entities: u32 = (&entities, &positions, &healths, &groups)
                    .join()
                    .map(|(entity, pos, health, group)| {
                        if entity != player
                            && group == &ENEMY
                            && (player_pos.0 - pos.0).magnitude_squared()
                                < mtm.combat_nearby_radius.powf(2.0)
                        {
                            (health.maximum() / mtm.combat_health_factor).ceil() as u32
                        } else {
                            0
                        }
                    })
                    .sum();

                if num_nearby_entities >= mtm.combat_nearby_high_thresh {
                    activity_state = MusicActivity::Combat(CombatIntensity::High);
                } else if num_nearby_entities >= mtm.combat_nearby_low_thresh {
                    activity_state = MusicActivity::Combat(CombatIntensity::Low);
                }
            }
        }

        // Override combat music with explore music if the player is dead
        if let Some(health) = healths.get(player) {
            if health.is_dead {
                activity_state = MusicActivity::Explore;
            }
        }

        let mut music_state = match self.last_activity {
            MusicState::Activity(prev) => {
                if prev != activity_state {
                    MusicState::Transition(prev, activity_state)
                } else {
                    MusicState::Activity(activity_state)
                }
            },
            MusicState::Transition(_, next) => {
                warn!("Transitioning: {:?}", self.last_activity);
                MusicState::Activity(next)
            },
        };

        let now = audio.get_clock_time().unwrap();

        let began_playing = *self.began_playing.get_or_insert(now);
        let last_interrupt_attempt = *self.last_interrupt_attempt.get_or_insert(now);
        let song_end = *self.song_end.get_or_insert(now);
        let time_since_began_playing = time_f64(now) - time_f64(began_playing);

        // TODO: Instead of a constant tick, make this a timer that starts only when
        // combat might end, providing a proper "buffer".
        // interrupt_delay dictates the time between attempted interrupts
        let interrupt = matches!(music_state, MusicState::Transition(_, _))
            && time_f64(now) - time_f64(last_interrupt_attempt) > mtm.interrupt_delay as f64;

        // Hack to end combat music since there is currently nothing that detects
        // transitions away
        if matches!(
            music_state,
            MusicState::Transition(
                MusicActivity::Combat(CombatIntensity::High),
                MusicActivity::Explore
            )
        ) {
            music_state = MusicState::Activity(MusicActivity::Explore)
        }

        if audio.music_enabled()
            && !self.soundtrack.tracks.is_empty()
            && (time_since_began_playing
                > time_f64(song_end) - time_f64(began_playing) // Amount of time between when the song ends and when it began playing
                || interrupt)
        {
            if time_since_began_playing > self.track_length as f64
                && self.last_activity
                    != MusicState::Activity(MusicActivity::Combat(CombatIntensity::High))
            {
                self.current_track = String::from("None");
                self.current_artist = String::from("None");
            }

            if interrupt {
                self.last_interrupt_attempt = Some(now);
                if let Ok(next_activity) =
                    self.play_random_track(audio, state, client, &music_state, &mut rng)
                {
                    trace!(
                        "pre-play_random_track: {:?} {:?}",
                        self.last_activity, music_state
                    );
                    self.last_activity = next_activity;
                }
            } else if music_state == MusicState::Activity(MusicActivity::Explore)
                || music_state
                    == MusicState::Transition(
                        MusicActivity::Explore,
                        MusicActivity::Combat(CombatIntensity::High),
                    )
            {
                // If current state is Explore, insert a gap now.
                if !self.is_gap {
                    self.gap_length = self.generate_silence_between_tracks(
                        audio.music_spacing,
                        client,
                        &music_state,
                        &mut rng,
                    );
                    self.gap_time = self.gap_length as f64;
                    self.song_end = audio.get_clock_time();
                    self.is_gap = true
                } else if self.gap_time < 0.0 {
                    // Gap time is up, play a track
                    // Hack to make combat situations not cancel explore music for now
                    if music_state
                        == MusicState::Transition(
                            MusicActivity::Explore,
                            MusicActivity::Combat(CombatIntensity::High),
                        )
                    {
                        music_state = MusicState::Activity(MusicActivity::Explore)
                    }
                    if let Ok(next_activity) =
                        self.play_random_track(audio, state, client, &music_state, &mut rng)
                    {
                        self.last_activity = next_activity;
                        self.gap_time = 0.0;
                        self.gap_length = 0.0;
                        self.is_gap = false;
                    }
                }
            } else if music_state
                == MusicState::Activity(MusicActivity::Combat(CombatIntensity::High))
            {
                // Keep playing! The track should loop automatically.
                self.began_playing = Some(now);
                self.song_end = Some(ClockTime::from_ticks_f64(
                    audio.get_clock().unwrap().id(),
                    time_f64(now) + self.loop_points.unwrap_or((0.0, 0.0)).1 as f64
                        - self.loop_points.unwrap_or((0.0, 0.0)).0 as f64,
                ));
            } else {
                trace!(
                    "pre-play_random_track: {:?} {:?}",
                    self.last_activity, music_state
                );
            }
        } else {
            if self.began_playing.is_none() {
                self.began_playing = Some(now)
            }
            if self.soundtrack.tracks.is_empty() {
                warn!("No tracks available to play")
            }
        }

        if time_since_began_playing > self.track_length as f64 {
            // Time remaining = Max time - (current time - time song ended)
            if self.is_gap {
                self.gap_time = (self.gap_length as f64) - (time_f64(now) - time_f64(song_end));
            }
        }
    }

    fn play_random_track(
        &mut self,
        audio: &mut AudioFrontend,
        state: &State,
        client: &Client,
        music_state: &MusicState,
        rng: &mut ThreadRng,
    ) -> Result<MusicState, String> {
        let is_dark = state.get_day_period().is_dark();
        let current_period_of_day = Self::get_current_day_period(is_dark);
        let current_weather = client.weather_at_player();
        let current_biome = client.current_biome();
        let current_site = client.current_site();

        // Filter the soundtrack in stages, so that we don't overprune it if there are
        // too many constraints. Returning Err(()) signals that we couldn't find
        // an appropriate track for the current state, and hence the state
        // machine for the activity shouldn't be updated.
        // First, filter out tracks not matching the timing, site, biome, and current
        // activity
        let mut maybe_tracks = self
            .soundtrack
            .tracks
            .iter()
            .filter(|track| {
                (match &track.timing {
                    Some(period_of_day) => period_of_day == &current_period_of_day,
                    None => true,
                }) && match &track.weather {
                    Some(weather) => weather == &current_weather.get_kind(),
                    None => true,
                }
            })
            .filter(|track| track.sites.iter().any(|s| s == &current_site))
            .filter(|track| {
                track.biomes.is_empty() || track.biomes.iter().any(|b| b.0 == current_biome)
            })
            .filter(|track| &track.music_state == music_state)
            .collect::<Vec<&SoundtrackItem>>();
        if maybe_tracks.is_empty() {
            let error_string = format!(
                "No tracks for {:?}, {:?}, {:?}, {:?}, {:?}",
                &current_period_of_day,
                &current_weather,
                &current_site,
                &current_biome,
                &music_state
            );
            return Err(error_string);
        }
        // Second, prevent playing the last track (when not in combat, because then it
        // needs to loop)
        if matches!(
            music_state,
            &MusicState::Activity(MusicActivity::Combat(CombatIntensity::High))
                | &MusicState::Transition(
                    MusicActivity::Combat(CombatIntensity::High),
                    MusicActivity::Explore
                )
        ) {
            let filtered_tracks: Vec<_> = maybe_tracks
                .iter()
                .filter(|track| track.title.eq(&self.last_track))
                .copied()
                .collect();
            if !filtered_tracks.is_empty() {
                maybe_tracks = filtered_tracks;
            }
        } else {
            let filtered_tracks: Vec<_> = maybe_tracks
                .iter()
                .filter(|track| !track.title.eq(&self.last_track))
                .filter(|track| !track.title.eq(&self.last_combat_track))
                .copied()
                .collect();
            if !filtered_tracks.is_empty() {
                maybe_tracks = filtered_tracks;
            }
        }

        // Randomly selects a track from the remaining tracks weighted based
        // on the biome
        let new_maybe_track = maybe_tracks.choose_weighted(rng, |track| {
            // If no biome is listed, the song is still added to the
            // rotation to allow for site specific songs to play
            // in any biome
            track
                .biomes
                .iter()
                .find(|b| b.0 == current_biome)
                .map_or(1.0, |b| (1.0_f32 / (b.1 as f32)))
        });
        debug!(
            "selecting new track for {:?}: {:?}",
            music_state, new_maybe_track
        );

        if let Ok(track) = new_maybe_track {
            let now = audio.get_clock_time().unwrap();
            self.last_track = String::from(&track.title);
            self.began_playing = Some(now);
            self.song_end = Some(ClockTime::from_ticks_f64(
                audio.get_clock().unwrap().id(),
                time_f64(now) + track.length as f64,
            ));
            self.track_length = track.length;
            self.gap_length = 0.0;
            if audio.music_enabled() {
                self.current_track = String::from(&track.title);
                self.current_artist = String::from(&track.artist.0);
            } else {
                self.current_track = String::from("None");
                self.current_artist = String::from("None");
            }

            let tag = if matches!(music_state, MusicState::Activity(MusicActivity::Explore)) {
                MusicChannelTag::Exploration
            } else {
                self.last_combat_track = String::from(&track.title);
                MusicChannelTag::Combat
            };
            audio.play_music(&track.path, tag, track.length);
            if tag == MusicChannelTag::Combat {
                audio.set_loop_points(
                    tag,
                    track.loop_points.unwrap_or((0.0, 0.0)).0,
                    track.loop_points.unwrap_or((0.0, 0.0)).1,
                );
                self.loop_points = track.loop_points
            } else {
                self.loop_points = None
            };

            if let Some(state) = track.activity_override {
                Ok(MusicState::Activity(state))
            } else {
                Ok(*music_state)
            }
        } else {
            Err(format!("{:?}", new_maybe_track))
        }
    }

    fn generate_silence_between_tracks(
        &self,
        spacing_multiplier: f32,
        client: &Client,
        music_state: &MusicState,
        rng: &mut ThreadRng,
    ) -> f32 {
        let mut silence_between_tracks_seconds: f32 = 0.0;
        if spacing_multiplier > f32::EPSILON {
            silence_between_tracks_seconds =
                if matches!(
                    music_state,
                    MusicState::Activity(MusicActivity::Explore)
                        | MusicState::Transition(
                            MusicActivity::Explore,
                            MusicActivity::Combat(CombatIntensity::High)
                        )
                ) && matches!(client.current_site(), SiteKindMeta::Settlement(_))
                {
                    rng.gen_range(120.0 * spacing_multiplier..180.0 * spacing_multiplier)
                } else if matches!(
                    music_state,
                    MusicState::Activity(MusicActivity::Explore)
                        | MusicState::Transition(
                            MusicActivity::Explore,
                            MusicActivity::Combat(CombatIntensity::High)
                        )
                ) && matches!(client.current_site(), SiteKindMeta::Dungeon(_))
                {
                    rng.gen_range(10.0 * spacing_multiplier..20.0 * spacing_multiplier)
                } else if matches!(
                    music_state,
                    MusicState::Activity(MusicActivity::Explore)
                        | MusicState::Transition(
                            MusicActivity::Explore,
                            MusicActivity::Combat(CombatIntensity::High)
                        )
                ) && matches!(client.current_site(), SiteKindMeta::Cave)
                {
                    rng.gen_range(20.0 * spacing_multiplier..40.0 * spacing_multiplier)
                } else if matches!(
                    music_state,
                    MusicState::Activity(MusicActivity::Explore)
                        | MusicState::Transition(
                            MusicActivity::Explore,
                            MusicActivity::Combat(CombatIntensity::High)
                        )
                ) {
                    rng.gen_range(120.0 * spacing_multiplier..240.0 * spacing_multiplier)
                } else if matches!(
                    music_state,
                    MusicState::Activity(MusicActivity::Combat(_)) | MusicState::Transition(_, _)
                ) {
                    0.0
                } else {
                    rng.gen_range(30.0 * spacing_multiplier..60.0 * spacing_multiplier)
                };
        }
        silence_between_tracks_seconds
    }

    fn get_current_day_period(is_dark: bool) -> DayPeriod {
        if is_dark {
            DayPeriod::Night
        } else {
            DayPeriod::Day
        }
    }

    pub fn current_track(&self) -> String { self.current_track.clone() }

    pub fn current_artist(&self) -> String { self.current_artist.clone() }

    pub fn reset_track(&mut self, audio: &mut AudioFrontend) {
        self.current_artist = String::from("None");
        self.current_track = String::from("None");
        self.gap_length = 1.0;
        self.gap_time = 1.0;
        self.is_gap = true;
        self.track_length = 0.0;
        self.began_playing = audio.get_clock_time();
        self.song_end = audio.get_clock_time();
    }

    /// Loads default soundtrack if no events are active. Otherwise, attempts to
    /// compile and load all active event soundtracks, falling back to default
    /// if they are empty.
    fn load_soundtrack_items(calendar: &Calendar) -> SoundtrackCollection<SoundtrackItem> {
        let mut soundtrack = SoundtrackCollection::default();
        // Loads default soundtrack if no events are active
        if calendar.events().len() == 0 {
            for track in SoundtrackCollection::load_expect("voxygen.audio.soundtrack")
                .read()
                .tracks
                .clone()
            {
                soundtrack.tracks.push(track)
            }
        } else {
            // Compiles event-specific soundtracks if any are active
            for event in calendar.events() {
                match event {
                    CalendarEvent::Halloween => {
                        for track in SoundtrackCollection::load_expect(
                            "voxygen.audio.calendar.halloween.soundtrack",
                        )
                        .read()
                        .tracks
                        .clone()
                        {
                            soundtrack.tracks.push(track)
                        }
                    },
                    CalendarEvent::Christmas => {
                        for track in SoundtrackCollection::load_expect(
                            "voxygen.audio.calendar.christmas.soundtrack",
                        )
                        .read()
                        .tracks
                        .clone()
                        {
                            soundtrack.tracks.push(track)
                        }
                    },
                    _ => {
                        for track in SoundtrackCollection::load_expect("voxygen.audio.soundtrack")
                            .read()
                            .tracks
                            .clone()
                        {
                            soundtrack.tracks.push(track)
                        }
                    },
                }
            }
        }
        // Fallback if events are active but give an empty tracklist
        if soundtrack.tracks.is_empty() {
            for track in SoundtrackCollection::load_expect("voxygen.audio.soundtrack")
                .read()
                .tracks
                .clone()
            {
                soundtrack.tracks.push(track)
            }
            soundtrack
        } else {
            soundtrack
        }
    }
}
impl assets::Asset for SoundtrackCollection<RawSoundtrackItem> {
    type Loader = assets::RonLoader;

    const EXTENSION: &'static str = "ron";
}

impl assets::Compound for SoundtrackCollection<SoundtrackItem> {
    fn load(_: assets::AnyCache, id: &assets::SharedString) -> Result<Self, assets::BoxedError> {
        let manifest: AssetHandle<SoundtrackCollection<RawSoundtrackItem>> = AssetExt::load(id)?;
        let mut soundtrack = SoundtrackCollection::default();
        for item in manifest.read().tracks.iter().cloned() {
            match item {
                RawSoundtrackItem::Individual(track) => soundtrack.tracks.push(track),
                RawSoundtrackItem::Segmented {
                    title,
                    timing,
                    weather,
                    biomes,
                    sites,
                    segments,
                    loop_points,
                    artist,
                } => {
                    for (path, length, music_state, activity_override) in segments.into_iter() {
                        soundtrack.tracks.push(SoundtrackItem {
                            title: title.clone(),
                            path,
                            length,
                            loop_points: Some(loop_points),
                            timing: timing.clone(),
                            weather,
                            biomes: biomes.clone(),
                            sites: sites.clone(),
                            music_state,
                            activity_override,
                            artist: artist.clone(),
                        });
                    }
                },
            }
        }
        Ok(soundtrack)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strum::IntoEnumIterator;

    #[test]
    fn test_load_soundtracks() {
        let _: AssetHandle<SoundtrackCollection<SoundtrackItem>> =
            SoundtrackCollection::load_expect("voxygen.audio.soundtrack");
        for event in CalendarEvent::iter() {
            match event {
                CalendarEvent::Halloween => {
                    let _: AssetHandle<SoundtrackCollection<SoundtrackItem>> =
                        SoundtrackCollection::load_expect(
                            "voxygen.audio.calendar.halloween.soundtrack",
                        );
                },
                CalendarEvent::Christmas => {
                    let _: AssetHandle<SoundtrackCollection<SoundtrackItem>> =
                        SoundtrackCollection::load_expect(
                            "voxygen.audio.calendar.christmas.soundtrack",
                        );
                },
                _ => {},
            }
        }
    }
}
