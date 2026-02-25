// ─── Smart Playlist Prompts ─────────────────────────────────────────────────

pub const INTENT_EXTRACTION_SYSTEM_PROMPT: &str = "\
<identity>
You are a music librarian assistant that interprets natural language playlist descriptions into structured search criteria.
</identity>

<instructions>
- Parse the user's playlist description into concrete music metadata filters
- Extract: genres, styles, moods, decade ranges, BPM ranges, and artist names
- Be generous with genre interpretation: \"chill jazz\" → genre jazz + mood relaxed
- For abstract prompts (\"music for a road trip\"), infer likely genres, moods, and tempos
- Respond with ONLY a JSON object, no explanation
</instructions>

<output_format>
{
  \"genres\": [\"string\"],
  \"moods\": [\"string\"],
  \"decade_start\": null or int,
  \"decade_end\": null or int,
  \"bpm_min\": null or int,
  \"bpm_max\": null or int,
  \"artists\": [\"string\"],
  \"exclude_genres\": [\"string\"],
  \"exclude_artists\": [\"string\"]
}
</output_format>";

pub const SMART_PLAYLIST_SYSTEM_PROMPT: &str = "\
<identity>
You are a music curator building a playlist from a personal music library. You have deep knowledge of music flow, energy arcs, and sonic compatibility.
</identity>

<instructions>
- Select and order tracks to create a cohesive listening experience matching the user's description
- Consider: mood progression, energy arc (build up then wind down), key compatibility, genre coherence, tempo flow
- Avoid clustering tracks from the same artist or album adjacently
- Aim for variety while maintaining thematic coherence
- Select the number of tracks requested (default 20 if not specified)
- Generate a short, evocative playlist title (2-5 words) and a one-sentence description
</instructions>

<output_format>
Respond with valid JSON only. No markdown, no code fences, no text outside the JSON.

{\"title\": \"string\", \"description\": \"string\", \"trackIds\": [\"id1\", \"id2\", ...]}
</output_format>";

pub const SMART_PLAYLIST_REFINE_SYSTEM_PROMPT: &str = "\
<identity>
You are a music curator refining an existing playlist based on user feedback. You have deep knowledge of music flow and sonic compatibility.
</identity>

<instructions>
- You are given the current playlist and a pool of additional candidate tracks
- Apply the user's refinement instruction: add tracks, change direction, adjust mood, etc.
- Maintain the qualities of the original playlist where they don't conflict with the refinement
- Return a complete updated track list (not just additions)
- Generate an updated title and description if the refinement significantly changes the playlist's character
</instructions>

<output_format>
Respond with valid JSON only. No markdown, no code fences, no text outside the JSON.

{\"title\": \"string\", \"description\": \"string\", \"trackIds\": [\"id1\", \"id2\", ...]}
</output_format>";

/// Compact representation of a track candidate for AI selection.
pub struct TrackCandidate {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub year: Option<i32>,
    pub bpm: Option<f64>,
    pub key: Option<String>,
    pub mood: Option<String>,
    pub duration_seconds: i32,
    pub album_moods: String,
    pub album_descriptors: String,
}

pub fn build_smart_playlist_prompt(prompt: &str, candidates: &[TrackCandidate], track_count: u32) -> String {
    let mut parts = Vec::new();
    parts.push(format!(
        "User request: \"{}\"\nSelect approximately {} tracks.\n",
        prompt, track_count
    ));
    parts.push("<candidates>".to_string());

    for c in candidates {
        let year_str = c.year.map(|y| y.to_string()).unwrap_or_default();
        let bpm_str = c.bpm.map(|b| format!("{:.0}", b)).unwrap_or_default();
        let key_str = c.key.as_deref().unwrap_or("");
        let mood_str = c.mood.as_deref().unwrap_or("");
        let mins = c.duration_seconds / 60;
        let secs = c.duration_seconds % 60;
        let mut xml = format!(
            "<track id=\"{}\"><title>{}</title><artist>{}</artist><album>{}</album><genre>{}</genre><year>{}</year><bpm>{}</bpm><key>{}</key><mood>{}</mood><duration>{}:{:02}</duration>",
            c.id, c.title, c.artist, c.album, c.genre, year_str, bpm_str, key_str, mood_str, mins, secs
        );
        if !c.album_moods.is_empty() {
            xml.push_str(&format!("<album_moods>{}</album_moods>", c.album_moods));
        }
        if !c.album_descriptors.is_empty() {
            xml.push_str(&format!("<album_descriptors>{}</album_descriptors>", c.album_descriptors));
        }
        xml.push_str("</track>");
        parts.push(xml);
    }

    parts.push("</candidates>".to_string());
    parts.push(String::new());
    parts.push("Select and order tracks, then return the JSON.".to_string());
    parts.join("\n")
}

pub fn build_smart_playlist_refine_prompt(
    refinement: &str,
    current_tracks: &[TrackCandidate],
    new_candidates: &[TrackCandidate],
    track_count: u32,
) -> String {
    let mut parts = Vec::new();
    parts.push(format!(
        "Refinement instruction: \"{}\"\nTarget approximately {} tracks.\n",
        refinement, track_count
    ));

    parts.push("<current_playlist>".to_string());
    for c in current_tracks {
        parts.push(format!(
            "<track id=\"{}\"><title>{}</title><artist>{}</artist><album>{}</album></track>",
            c.id, c.title, c.artist, c.album
        ));
    }
    parts.push("</current_playlist>".to_string());

    parts.push(String::new());
    parts.push("<additional_candidates>".to_string());
    for c in new_candidates {
        let year_str = c.year.map(|y| y.to_string()).unwrap_or_default();
        let bpm_str = c.bpm.map(|b| format!("{:.0}", b)).unwrap_or_default();
        let key_str = c.key.as_deref().unwrap_or("");
        let mood_str = c.mood.as_deref().unwrap_or("");
        let mins = c.duration_seconds / 60;
        let secs = c.duration_seconds % 60;
        let mut xml = format!(
            "<track id=\"{}\"><title>{}</title><artist>{}</artist><album>{}</album><genre>{}</genre><year>{}</year><bpm>{}</bpm><key>{}</key><mood>{}</mood><duration>{}:{:02}</duration>",
            c.id, c.title, c.artist, c.album, c.genre, year_str, bpm_str, key_str, mood_str, mins, secs
        );
        if !c.album_moods.is_empty() {
            xml.push_str(&format!("<album_moods>{}</album_moods>", c.album_moods));
        }
        if !c.album_descriptors.is_empty() {
            xml.push_str(&format!("<album_descriptors>{}</album_descriptors>", c.album_descriptors));
        }
        xml.push_str("</track>");
        parts.push(xml);
    }
    parts.push("</additional_candidates>".to_string());

    parts.push(String::new());
    parts.push("Return the complete updated playlist as JSON.".to_string());
    parts.join("\n")
}
