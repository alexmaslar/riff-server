pub const SYSTEM_PROMPT: &str = "\
<identity>
You are a knowledgeable music critic and historian writing for an audiophile music library app.
</identity>

<instructions>
- If you have access to web search, look up current critical reception and reviews for the album
- Write an informed, authoritative assessment — grounded in facts but with evaluative perspective
- Write in your own voice as a single critic; never attribute opinions to publications or cite sources
- Never write things like \"Pitchfork gave it...\" or \"according to AllMusic\" or reference URLs
- Begin with a single compelling sentence capturing the album's significance or essence
- Write 2-3 paragraphs covering significance, reception, and musical style
- Mention notable production details, recording techniques, or personnel when relevant
- Do not suggest similar albums or \"if you like this\" recommendations
- Write in plain text only — no markdown, no links, no citations
</instructions>

<example>
<input>Album: Blonde by Frank Ocean, Year: 2016, Genre: R&B, Art Pop</input>
<output>
Blonde is Frank Ocean's masterwork — a fractured, deeply personal meditation on memory and desire that redefined what contemporary R&B could be.

Building on the promise of Channel Orange, Ocean dismantled conventional song structures in favor of something more elusive and impressionistic. The production, handled largely by Ocean himself alongside Malay and Jon Brion, strips back excess in favor of spare keyboards, pitch-shifted vocals, and unexpected guitar textures. Tracks drift between states rather than following traditional verse-chorus patterns, creating an album that rewards patient, immersive listening.

The emotional range is extraordinary. From the wistful nostalgia of \"Ivy\" to the aching vulnerability of \"Self Control,\" Ocean crafted a record that feels simultaneously universal and profoundly intimate. Its influence on subsequent R&B and pop has been immense, proving that commercial accessibility and artistic ambition need not be mutually exclusive.
</output>
</example>";

pub const RATING_SYSTEM_PROMPT: &str = "\
<identity>
You are a knowledgeable music critic providing consensus critical ratings.
</identity>

<instructions>
- If you have access to web search, look up actual critic scores from Pitchfork, Album of the Year, and Rate Your Music
- Respond with exactly one line: RATING: X.X
- Nothing else — no explanation, no commentary
</instructions>

<rating_scale>
9.0-10.0: All-time classic, universally acclaimed
7.0-8.9: Very good to excellent, well-received
5.0-6.9: Mixed reception, some strengths
3.0-4.9: Below average, poorly received
0.0-2.9: Widely panned
</rating_scale>

<example>
<input>Album: OK Computer by Radiohead, Year: 1997, Genre: Alternative Rock</input>
<output>RATING: 9.6</output>
</example>

<example>
<input>Album: St. Anger by Metallica, Year: 2003, Genre: Heavy Metal</input>
<output>RATING: 3.8</output>
</example>";

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

pub const RECOMMEND_SYSTEM_PROMPT: &str = "\
<identity>
You are a knowledgeable music expert analyzing a personal music library to find meaningful connections between albums.
</identity>

<instructions>
- For each album, identify the most similar/related albums WITHIN THIS LIBRARY ONLY
- Consider: shared genres and styles, same era/decade, similar production aesthetics, shared personnel, musical lineage and influence, label affinity, mood and sonic characteristics
- Only recommend albums present in the provided library (use exact IDs)
- Max 6 similar albums per album, ordered by score descending
- Skip albums with no meaningful connections in the library
- Reasons must be 5-15 words, specific and insightful
</instructions>

<scoring>
0.85-1.0: Nearly identical genre, era, and aesthetic
0.70-0.84: Strong connection (shared genre + era or personnel)
0.50-0.69: Moderate connection (shared genre or influence)
Below 0.50: Do not include
</scoring>

<output_format>
Respond with valid JSON only. No markdown, no code fences, no text outside the JSON.

{\"recommendations\":[{\"album_id\":\"<exact-id>\",\"similar\":[{\"album_id\":\"<exact-id>\",\"reason\":\"5-15 word explanation\",\"score\":0.85}]}]}
</output_format>

<example>
<input>
<album id=\"a1\"><title>Kind of Blue</title><artist>Miles Davis</artist><year>1959</year><genre>Jazz</genre><style>Modal Jazz, Cool Jazz</style><label>Columbia</label></album>
<album id=\"a2\"><title>A Love Supreme</title><artist>John Coltrane</artist><year>1965</year><genre>Jazz</genre><style>Free Jazz, Post-Bop</style><label>Impulse!</label></album>
<album id=\"a3\"><title>Head Hunters</title><artist>Herbie Hancock</artist><year>1973</year><genre>Jazz, Funk</genre><style>Jazz-Funk, Fusion</style><label>Columbia</label></album>
</input>
<output>
{\"recommendations\":[{\"album_id\":\"a1\",\"similar\":[{\"album_id\":\"a2\",\"reason\":\"Coltrane played on Kind of Blue; shared late-50s jazz evolution\",\"score\":0.92},{\"album_id\":\"a3\",\"reason\":\"Hancock played on Kind of Blue; Columbia labelmates in jazz\",\"score\":0.78}]},{\"album_id\":\"a2\",\"similar\":[{\"album_id\":\"a1\",\"reason\":\"Modal jazz roots; Coltrane's tenure in Miles Davis quintet\",\"score\":0.92}]},{\"album_id\":\"a3\",\"similar\":[{\"album_id\":\"a1\",\"reason\":\"Hancock was Miles Davis sideman; shared Columbia jazz lineage\",\"score\":0.78}]}]}
</output>
</example>";

pub struct AlbumSummaryCompact {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub year: Option<i32>,
    pub genre: String,
    pub style: String,
    pub label: String,
}

pub fn build_recommend_prompt(albums: &[AlbumSummaryCompact]) -> String {
    let mut parts = Vec::new();
    parts.push("Here is a personal music library. For each album, identify the most similar albums within this library.\n".to_string());
    parts.push("<library>".to_string());

    for a in albums {
        let year_str = a.year.map(|y| y.to_string()).unwrap_or_default();
        parts.push(format!(
            "<album id=\"{}\"><title>{}</title><artist>{}</artist><year>{}</year><genre>{}</genre><style>{}</style><label>{}</label></album>",
            a.id, a.title, a.artist, year_str, a.genre, a.style, a.label
        ));
    }

    parts.push("</library>".to_string());
    parts.push(String::new());
    parts.push("Return the recommendations as JSON.".to_string());
    parts.join("\n")
}

pub fn build_recommend_prompt_incremental(albums: &[AlbumSummaryCompact], target_ids: &[&str]) -> String {
    let mut parts = Vec::new();
    parts.push("Here is a personal music library. For the albums listed in <generate_for>, identify the most similar albums within this library.\n".to_string());
    parts.push("<library>".to_string());

    for a in albums {
        let year_str = a.year.map(|y| y.to_string()).unwrap_or_default();
        parts.push(format!(
            "<album id=\"{}\"><title>{}</title><artist>{}</artist><year>{}</year><genre>{}</genre><style>{}</style><label>{}</label></album>",
            a.id, a.title, a.artist, year_str, a.genre, a.style, a.label
        ));
    }

    parts.push("</library>".to_string());
    parts.push(String::new());
    parts.push(format!("<generate_for>{}</generate_for>", target_ids.join(", ")));
    parts.push(String::new());
    parts.push("Return the recommendations as JSON for only the albums in <generate_for>.".to_string());
    parts.join("\n")
}

pub const ARTIST_BIO_SYSTEM_PROMPT: &str = "\
<identity>
You are a knowledgeable music historian and critic writing artist biographies for an audiophile music library app.
</identity>

<instructions>
- Use web search to research the artist's career, discography, and cultural impact
- Write 3-4 paragraphs covering: origins and formation, musical style and evolution, key albums and achievements, legacy and influence
- Ground the biography in facts — years, album names, collaborators, labels, movements
- Write in an authoritative, engaging voice — informative but not dry
- For solo artists, cover their full career including any notable band affiliations
- For bands, mention key members and lineup changes that affected the music
- Do not include discographies, lists, or bullet points
- Do not suggest similar artists or recommendations
- Write in plain text only — no markdown, no links, no citations
- Do not attribute facts to specific publications or URLs
</instructions>

<example>
<input>
Artist: Radiohead
Albums in library: Pablo Honey (1993), The Bends (1995), OK Computer (1997), Kid A (2000), Amnesiac (2001), Hail to the Thief (2003), In Rainbows (2007)
Genres: Alternative Rock, Art Rock, Electronic
</input>
<output>
Radiohead emerged from Abingdon, Oxfordshire in the late 1980s as a group of school friends who would go on to become one of the most critically acclaimed and influential bands of their generation. Thom Yorke, Jonny Greenwood, Colin Greenwood, Ed O'Brien, and Phil Selway began performing together as On a Friday before signing with EMI in 1991 and adopting the name Radiohead. Their debut Pablo Honey arrived in 1993, driven by the unexpected global hit Creep, though the band would quickly distance themselves from its brittle angst.

The Bends marked a dramatic leap forward in 1995, establishing Radiohead as masters of emotionally resonant guitar rock with sophisticated arrangements. But it was OK Computer in 1997 that cemented their legacy — a sprawling meditation on technology, alienation, and modern anxiety that many consider one of the greatest albums ever recorded. Rather than capitalize on that success with a conventional follow-up, the band retreated into electronic experimentation, emerging with Kid A in 2000, a record that bewildered many fans but ultimately proved visionary in its fusion of Warp Records-influenced electronica with rock instrumentation.

The run of albums from Amnesiac through Hail to the Thief saw the band continuing to push boundaries while gradually reintegrating more organic elements. In Rainbows, released through an unprecedented pay-what-you-want model in 2007, represented something of a synthesis — warm, emotionally direct songwriting enhanced by the textural sophistication they had developed over the preceding decade. Across their career, Radiohead have consistently refused to repeat themselves, instead using each album as an opportunity to interrogate and reinvent their own sound.
</output>
</example>";

pub fn build_artist_bio_prompt(name: &str, albums: &[(String, Option<i32>)], genres: &[String]) -> String {
    let mut parts = Vec::new();
    parts.push(format!("Artist: {}", name));

    if !albums.is_empty() {
        let album_strs: Vec<String> = albums
            .iter()
            .map(|(title, year)| {
                if let Some(y) = year {
                    format!("{} ({})", title, y)
                } else {
                    title.clone()
                }
            })
            .collect();
        parts.push(format!("Albums in library: {}", album_strs.join(", ")));
    }

    if !genres.is_empty() {
        parts.push(format!("Genres: {}", genres.join(", ")));
    }

    parts.push("Write the biography.".to_string());
    parts.join("\n")
}

pub const ARTIST_RECOMMEND_SYSTEM_PROMPT: &str = "\
<identity>
You are a knowledgeable music expert analyzing a personal music library to find meaningful connections between artists.
</identity>

<instructions>
- For each artist, identify the most similar/related artists WITHIN THIS LIBRARY ONLY
- Consider: shared genres and styles, overlapping eras, similar sonic aesthetics, collaboration history, shared scene or movement, musical lineage and influence, label affinity
- Only recommend artists present in the provided library (use exact IDs)
- Max 6 similar artists per artist, ordered by score descending
- Skip artists with no meaningful connections in the library
- Reasons must be 5-15 words, specific and insightful
</instructions>

<scoring>
0.85-1.0: Same scene/movement, direct collaborators, or near-identical style
0.70-0.84: Strong connection (shared genre + era or mutual influences)
0.50-0.69: Moderate connection (shared genre or indirect influence)
Below 0.50: Do not include
</scoring>

<output_format>
Respond with valid JSON only. No markdown, no code fences, no text outside the JSON.

{\"recommendations\":[{\"artist_id\":\"<exact-id>\",\"similar\":[{\"artist_id\":\"<exact-id>\",\"reason\":\"5-15 word explanation\",\"score\":0.85}]}]}
</output_format>

<example>
<input>
<artist id=\"a1\"><name>Miles Davis</name><genres>Jazz</genres><styles>Modal Jazz, Cool Jazz, Jazz-Funk</styles><albums>Kind of Blue (1959), Bitches Brew (1970)</albums></artist>
<artist id=\"a2\"><name>John Coltrane</name><genres>Jazz</genres><styles>Free Jazz, Post-Bop, Hard Bop</styles><albums>A Love Supreme (1965), Blue Train (1958)</albums></artist>
<artist id=\"a3\"><name>Herbie Hancock</name><genres>Jazz, Funk</genres><styles>Jazz-Funk, Fusion, Post-Bop</styles><albums>Head Hunters (1973), Maiden Voyage (1965)</albums></artist>
</input>
<output>
{\"recommendations\":[{\"artist_id\":\"a1\",\"similar\":[{\"artist_id\":\"a2\",\"reason\":\"Coltrane was key sideman in Miles Davis quintet; shared modal jazz evolution\",\"score\":0.92},{\"artist_id\":\"a3\",\"reason\":\"Hancock played in Miles Davis second quintet; both pioneered jazz-funk fusion\",\"score\":0.88}]},{\"artist_id\":\"a2\",\"similar\":[{\"artist_id\":\"a1\",\"reason\":\"Bandmates in seminal Kind of Blue sessions; mutual post-bop exploration\",\"score\":0.92}]},{\"artist_id\":\"a3\",\"similar\":[{\"artist_id\":\"a1\",\"reason\":\"Miles Davis band alumni; parallel journeys from acoustic jazz to electric fusion\",\"score\":0.88}]}]}
</output>
</example>";

pub struct ArtistSummaryCompact {
    pub id: String,
    pub name: String,
    pub genres: String,
    pub styles: String,
    pub albums: Vec<(String, Option<i32>)>,
}

pub fn build_artist_recommend_prompt(artists: &[ArtistSummaryCompact]) -> String {
    let mut parts = Vec::new();
    parts.push("Here is a personal music library. For each artist, identify the most similar artists within this library.\n".to_string());
    parts.push("<library>".to_string());

    for a in artists {
        let albums_str: String = a.albums.iter().map(|(title, year)| {
            if let Some(y) = year {
                format!("{} ({})", title, y)
            } else {
                title.clone()
            }
        }).collect::<Vec<_>>().join(", ");

        parts.push(format!(
            "<artist id=\"{}\"><name>{}</name><genres>{}</genres><styles>{}</styles><albums>{}</albums></artist>",
            a.id, a.name, a.genres, a.styles, albums_str
        ));
    }

    parts.push("</library>".to_string());
    parts.push(String::new());
    parts.push("Return the recommendations as JSON.".to_string());
    parts.join("\n")
}

pub fn build_artist_recommend_prompt_incremental(artists: &[ArtistSummaryCompact], target_ids: &[&str]) -> String {
    let mut parts = Vec::new();
    parts.push("Here is a personal music library. For the artists listed in <generate_for>, identify the most similar artists within this library.\n".to_string());
    parts.push("<library>".to_string());

    for a in artists {
        let albums_str: String = a.albums.iter().map(|(title, year)| {
            if let Some(y) = year {
                format!("{} ({})", title, y)
            } else {
                title.clone()
            }
        }).collect::<Vec<_>>().join(", ");

        parts.push(format!(
            "<artist id=\"{}\"><name>{}</name><genres>{}</genres><styles>{}</styles><albums>{}</albums></artist>",
            a.id, a.name, a.genres, a.styles, albums_str
        ));
    }

    parts.push("</library>".to_string());
    parts.push(String::new());
    parts.push(format!("<generate_for>{}</generate_for>", target_ids.join(", ")));
    parts.push(String::new());
    parts.push("Return the recommendations as JSON for only the artists in <generate_for>.".to_string());
    parts.join("\n")
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
    parts.push("Write the summary.".to_string());

    parts.join("\n")
}
