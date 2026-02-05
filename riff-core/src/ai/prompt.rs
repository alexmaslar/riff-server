pub const SYSTEM_PROMPT: &str = "\
You are a knowledgeable music critic and historian. Write album summaries for a music \
library application. Your summaries should be informative, engaging, and written for \
audiophiles and music enthusiasts.

Use web search to look up current critical reception, ratings, and reviews for the album. \
This is especially important for recent releases that may be beyond your training data.

Requirements:
- On the very first line, provide a critic consensus rating from 0.0 to 10.0 (one decimal place) \
reflecting the album's critical reception across sources like Pitchfork, Album of the Year, and \
Rate Your Music. Search the web to find actual scores when possible. Format exactly: RATING: X.X
- Then write the summary starting from the next line
- Write the summary as your own original review. Use the research to inform your perspective, \
but write in your own voice as a critic — do not attribute opinions to publications or cite sources. \
Never write things like \"Pitchfork gave it...\" or \"according to AllMusic\" or reference URLs. \
The reader should feel like they are reading a single knowledgeable critic's take, not a meta-review.
- Begin with a single compelling sentence that captures the album's significance or essence
- Write 2-3 paragraphs
- Cover the album's significance, how it was received, and musical style
- Mention notable production details, recording techniques, or personnel when relevant
- Do not suggest similar albums or \"if you like this\" recommendations
- Write in plain text only, no markdown formatting, no links, no citations
- Be concise and factual";

pub const RATING_SYSTEM_PROMPT: &str = "\
You are a knowledgeable music critic. Provide a critic consensus rating for the given album \
on a scale from 0.0 to 10.0 (one decimal place), reflecting its critical reception across \
sources like Pitchfork, Album of the Year, and Rate Your Music.

Search the web to find actual critic scores when possible, especially for recent releases.

Respond with exactly one line in this format: RATING: X.X

Nothing else.";

pub fn build_rating_prompt(title: &str, artist: &str, year: Option<i32>, genre: &[String]) -> String {
    let mut parts = Vec::new();
    parts.push(format!("Album: {} by {}", title, artist));
    if let Some(y) = year {
        parts.push(format!("Year: {}", y));
    }
    if !genre.is_empty() {
        parts.push(format!("Genre: {}", genre.join(", ")));
    }
    parts.push("Provide the rating as RATING: X.X".to_string());
    parts.join("\n")
}

pub struct AlbumContext {
    pub title: String,
    pub artist: String,
    pub year: Option<i32>,
    pub genre: Vec<String>,
    pub style: Vec<String>,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    pub country: Option<String>,
    pub release_notes: Option<String>,
    pub tracks: Vec<TrackInfo>,
    pub credits: Vec<CreditInfo>,
}

pub struct TrackInfo {
    pub number: i32,
    pub disc: i32,
    pub title: String,
    pub duration_seconds: i32,
}

pub struct CreditInfo {
    pub name: String,
    pub role: String,
}

pub fn build_album_prompt(ctx: &AlbumContext) -> String {
    let mut parts = Vec::new();

    parts.push(format!("Album: {} by {}", ctx.title, ctx.artist));

    if let Some(year) = ctx.year {
        parts.push(format!("Year: {}", year));
    }
    if !ctx.genre.is_empty() {
        parts.push(format!("Genre: {}", ctx.genre.join(", ")));
    }
    if !ctx.style.is_empty() {
        parts.push(format!("Style: {}", ctx.style.join(", ")));
    }
    if let Some(label) = &ctx.label {
        let mut label_line = format!("Label: {}", label);
        if let Some(catno) = &ctx.catalog_number {
            label_line.push_str(&format!(" ({})", catno));
        }
        parts.push(label_line);
    }
    if let Some(country) = &ctx.country {
        parts.push(format!("Country: {}", country));
    }

    if !ctx.tracks.is_empty() {
        parts.push(String::new());
        parts.push("Track listing:".to_string());
        for t in &ctx.tracks {
            let mins = t.duration_seconds / 60;
            let secs = t.duration_seconds % 60;
            let prefix = if ctx.tracks.iter().any(|tr| tr.disc > 1) {
                format!("{}-{:02}", t.disc, t.number)
            } else {
                format!("{:02}", t.number)
            };
            parts.push(format!("  {}. {} ({}:{:02})", prefix, t.title, mins, secs));
        }
    }

    if !ctx.credits.is_empty() {
        parts.push(String::new());
        parts.push("Credits:".to_string());
        for c in &ctx.credits {
            parts.push(format!("  {} - {}", c.name, c.role));
        }
    }

    if let Some(notes) = &ctx.release_notes {
        if !notes.is_empty() {
            parts.push(String::new());
            parts.push(format!("Release notes: {}", notes));
        }
    }

    parts.push(String::new());
    parts.push("Provide the rating on the first line as RATING: X.X, then the summary.".to_string());

    parts.join("\n")
}
