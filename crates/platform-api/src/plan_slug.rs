//! `B7t` / `HB` / `ynt` (2.1.266 `src_159374504.js`) — the plan-file slug.
//!
//! Upstream names a session's plan file with a random, human-readable slug
//! rather than the session id: `getPlanSlug(sessionId, seed)` returns
//! `${adjective}-${verb}-${noun}` (`B7t`), or `${slugify(seed)}-${adjective}-${noun}`
//! (`ynt` + `HB`) when a seed is supplied, retrying up to
//! [`SLUG_COLLISION_ATTEMPTS`] times against the plans directory.
//!
//! The three wordlists below are extracted from the executable, never retyped:
//! `~/.claude/oracle-chunks/notes/plan-goal-2.1.266/gen_plan_slug.py`.
//!
//! ## Divergence (reason)
//!
//! `g(n)` is `randomBytes(4).readUInt32BE(0) % n`. The port draws those four
//! bytes from a v4 UUID's first four bytes (fully random — the version and
//! variant bits live in bytes 6 and 8), so `platform-api` needs no RNG
//! dependency of its own. The modulo bias is upstream's, kept as-is.

/// `t` — the adjective list.
static ADJECTIVES: &[&str] = &[
    "abundant",
    "ancient",
    "bright",
    "calm",
    "cheerful",
    "clever",
    "cozy",
    "curious",
    "dapper",
    "dazzling",
    "deep",
    "delightful",
    "eager",
    "elegant",
    "enchanted",
    "fancy",
    "fluffy",
    "gentle",
    "gleaming",
    "golden",
    "graceful",
    "happy",
    "hidden",
    "humble",
    "jolly",
    "joyful",
    "keen",
    "kind",
    "lively",
    "lovely",
    "lucky",
    "luminous",
    "magical",
    "majestic",
    "mellow",
    "merry",
    "mighty",
    "misty",
    "noble",
    "peaceful",
    "playful",
    "polished",
    "precious",
    "proud",
    "quiet",
    "quirky",
    "radiant",
    "rosy",
    "serene",
    "shiny",
    "silly",
    "sleepy",
    "smooth",
    "snazzy",
    "snug",
    "snuggly",
    "soft",
    "sparkling",
    "spicy",
    "splendid",
    "sprightly",
    "starry",
    "steady",
    "sunny",
    "swift",
    "tender",
    "tidy",
    "toasty",
    "tranquil",
    "twinkly",
    "valiant",
    "vast",
    "velvet",
    "vivid",
    "warm",
    "whimsical",
    "wild",
    "wise",
    "witty",
    "wondrous",
    "zany",
    "zesty",
    "zippy",
    "breezy",
    "bubbly",
    "buzzing",
    "cheeky",
    "cosmic",
    "cozy",
    "crispy",
    "crystalline",
    "cuddly",
    "drifting",
    "dreamy",
    "effervescent",
    "ethereal",
    "fizzy",
    "flickering",
    "floating",
    "floofy",
    "fluttering",
    "foamy",
    "frolicking",
    "fuzzy",
    "giggly",
    "glimmering",
    "glistening",
    "glittery",
    "glowing",
    "goofy",
    "groovy",
    "harmonic",
    "hazy",
    "humming",
    "iridescent",
    "jaunty",
    "jazzy",
    "jiggly",
    "melodic",
    "moonlit",
    "mossy",
    "nifty",
    "peppy",
    "prancy",
    "purrfect",
    "purring",
    "quizzical",
    "rippling",
    "rustling",
    "shimmering",
    "shimmying",
    "snappy",
    "snoopy",
    "squishy",
    "swirling",
    "ticklish",
    "tingly",
    "twinkling",
    "velvety",
    "wiggly",
    "wobbly",
    "woolly",
    "zazzy",
    "abstract",
    "adaptive",
    "agile",
    "async",
    "atomic",
    "binary",
    "cached",
    "compiled",
    "composed",
    "compressed",
    "concurrent",
    "cryptic",
    "curried",
    "declarative",
    "delegated",
    "distributed",
    "dynamic",
    "eager",
    "elegant",
    "encapsulated",
    "enumerated",
    "eventual",
    "expressive",
    "federated",
    "functional",
    "generic",
    "greedy",
    "hashed",
    "idempotent",
    "immutable",
    "imperative",
    "indexed",
    "inherited",
    "iterative",
    "lazy",
    "lexical",
    "linear",
    "linked",
    "logical",
    "memoized",
    "modular",
    "mutable",
    "nested",
    "optimized",
    "parallel",
    "parsed",
    "partitioned",
    "piped",
    "polymorphic",
    "pure",
    "reactive",
    "recursive",
    "refactored",
    "reflective",
    "replicated",
    "resilient",
    "robust",
    "scalable",
    "sequential",
    "serialized",
    "sharded",
    "sorted",
    "staged",
    "stateful",
    "stateless",
    "streamed",
    "structured",
    "synchronous",
    "synthetic",
    "temporal",
    "transient",
    "typed",
    "unified",
    "validated",
    "vectorized",
    "virtual",
];

/// `s` — the gerund list, used only by the 3-word form.
static VERBS: &[&str] = &[
    "baking",
    "beaming",
    "booping",
    "bouncing",
    "brewing",
    "bubbling",
    "chasing",
    "churning",
    "coalescing",
    "conjuring",
    "cooking",
    "crafting",
    "crunching",
    "cuddling",
    "dancing",
    "dazzling",
    "discovering",
    "doodling",
    "dreaming",
    "drifting",
    "enchanting",
    "exploring",
    "finding",
    "floating",
    "fluttering",
    "foraging",
    "forging",
    "frolicking",
    "gathering",
    "giggling",
    "gliding",
    "greeting",
    "growing",
    "hatching",
    "herding",
    "honking",
    "hopping",
    "hugging",
    "humming",
    "imagining",
    "inventing",
    "jingling",
    "juggling",
    "jumping",
    "kindling",
    "knitting",
    "launching",
    "leaping",
    "mapping",
    "marinating",
    "meandering",
    "mixing",
    "moseying",
    "munching",
    "napping",
    "nibbling",
    "noodling",
    "orbiting",
    "painting",
    "percolating",
    "petting",
    "plotting",
    "pondering",
    "popping",
    "prancing",
    "purring",
    "puzzling",
    "questing",
    "riding",
    "roaming",
    "rolling",
    "sauteeing",
    "scribbling",
    "seeking",
    "shimmying",
    "singing",
    "skipping",
    "sleeping",
    "snacking",
    "sniffing",
    "snuggling",
    "soaring",
    "sparking",
    "spinning",
    "splashing",
    "sprouting",
    "squishing",
    "stargazing",
    "stirring",
    "strolling",
    "swimming",
    "swinging",
    "tickling",
    "tinkering",
    "toasting",
    "tumbling",
    "twirling",
    "waddling",
    "wandering",
    "watching",
    "weaving",
    "whistling",
    "wibbling",
    "wiggling",
    "wishing",
    "wobbling",
    "wondering",
    "yawning",
    "zooming",
];

/// `l` — the noun list.
static NOUNS: &[&str] = &[
    "aurora",
    "avalanche",
    "blossom",
    "breeze",
    "brook",
    "bubble",
    "canyon",
    "cascade",
    "cloud",
    "clover",
    "comet",
    "coral",
    "cosmos",
    "creek",
    "crescent",
    "crystal",
    "dawn",
    "dewdrop",
    "dusk",
    "eclipse",
    "ember",
    "feather",
    "fern",
    "firefly",
    "flame",
    "flurry",
    "fog",
    "forest",
    "frost",
    "galaxy",
    "garden",
    "glacier",
    "glade",
    "grove",
    "harbor",
    "horizon",
    "island",
    "lagoon",
    "lake",
    "leaf",
    "lightning",
    "meadow",
    "meteor",
    "mist",
    "moon",
    "moonbeam",
    "mountain",
    "nebula",
    "nova",
    "ocean",
    "orbit",
    "pebble",
    "petal",
    "pine",
    "planet",
    "pond",
    "puddle",
    "quasar",
    "rain",
    "rainbow",
    "reef",
    "ripple",
    "river",
    "shore",
    "sky",
    "snowflake",
    "spark",
    "spring",
    "star",
    "stardust",
    "starlight",
    "storm",
    "stream",
    "summit",
    "sun",
    "sunbeam",
    "sunrise",
    "sunset",
    "thunder",
    "tide",
    "twilight",
    "valley",
    "volcano",
    "waterfall",
    "wave",
    "willow",
    "wind",
    "alpaca",
    "axolotl",
    "badger",
    "bear",
    "beaver",
    "bee",
    "bird",
    "bumblebee",
    "bunny",
    "cat",
    "chipmunk",
    "crab",
    "crane",
    "deer",
    "dolphin",
    "dove",
    "dragon",
    "dragonfly",
    "duckling",
    "eagle",
    "elephant",
    "falcon",
    "finch",
    "flamingo",
    "fox",
    "frog",
    "giraffe",
    "goose",
    "hamster",
    "hare",
    "heron",
    "hippo",
    "hummingbird",
    "jellyfish",
    "kitten",
    "koala",
    "ladybug",
    "lark",
    "lemur",
    "llama",
    "lobster",
    "lynx",
    "magpie",
    "meerkat",
    "moth",
    "nautilus",
    "newt",
    "octopus",
    "otter",
    "owl",
    "panda",
    "parrot",
    "peacock",
    "pelican",
    "penguin",
    "phoenix",
    "piglet",
    "platypus",
    "pony",
    "possum",
    "puffin",
    "puppy",
    "quail",
    "quokka",
    "rabbit",
    "raccoon",
    "raven",
    "robin",
    "salamander",
    "seahorse",
    "seal",
    "sloth",
    "snail",
    "sparrow",
    "sphinx",
    "squid",
    "squirrel",
    "starfish",
    "swan",
    "tiger",
    "toucan",
    "turtle",
    "unicorn",
    "walrus",
    "whale",
    "wolf",
    "wombat",
    "wren",
    "yeti",
    "zebra",
    "acorn",
    "anchor",
    "balloon",
    "beacon",
    "biscuit",
    "blanket",
    "bonbon",
    "book",
    "boot",
    "cake",
    "candle",
    "candy",
    "castle",
    "charm",
    "clock",
    "cocoa",
    "cookie",
    "crayon",
    "crown",
    "cupcake",
    "donut",
    "dream",
    "fairy",
    "fiddle",
    "flask",
    "flute",
    "fountain",
    "gadget",
    "gem",
    "gizmo",
    "globe",
    "goblet",
    "hammock",
    "harp",
    "haven",
    "hearth",
    "honey",
    "journal",
    "kazoo",
    "kettle",
    "key",
    "kite",
    "lantern",
    "lemon",
    "lighthouse",
    "locket",
    "lollipop",
    "mango",
    "map",
    "marble",
    "marshmallow",
    "melody",
    "mitten",
    "mochi",
    "muffin",
    "music",
    "nest",
    "noodle",
    "oasis",
    "origami",
    "pancake",
    "parasol",
    "peach",
    "pearl",
    "pebble",
    "pie",
    "pillow",
    "pinwheel",
    "pixel",
    "pizza",
    "plum",
    "popcorn",
    "pretzel",
    "prism",
    "pudding",
    "pumpkin",
    "puzzle",
    "quiche",
    "quill",
    "quilt",
    "riddle",
    "rocket",
    "rose",
    "scone",
    "scroll",
    "shell",
    "sketch",
    "snowglobe",
    "sonnet",
    "sparkle",
    "spindle",
    "sprout",
    "sundae",
    "swing",
    "taco",
    "teacup",
    "teapot",
    "thimble",
    "toast",
    "token",
    "tome",
    "tower",
    "treasure",
    "treehouse",
    "trinket",
    "truffle",
    "tulip",
    "umbrella",
    "waffle",
    "wand",
    "whisper",
    "whistle",
    "widget",
    "wreath",
    "zephyr",
    "abelson",
    "adleman",
    "aho",
    "allen",
    "babbage",
    "bachman",
    "backus",
    "barto",
    "bengio",
    "bentley",
    "blum",
    "boole",
    "brooks",
    "catmull",
    "cerf",
    "cherny",
    "church",
    "clarke",
    "cocke",
    "codd",
    "conway",
    "cook",
    "corbato",
    "cray",
    "curry",
    "dahl",
    "diffie",
    "dijkstra",
    "dongarra",
    "eich",
    "emerson",
    "engelbart",
    "feigenbaum",
    "floyd",
    "gosling",
    "graham",
    "gray",
    "hamming",
    "hanrahan",
    "hartmanis",
    "hejlsberg",
    "hellman",
    "hennessy",
    "hickey",
    "hinton",
    "hoare",
    "hollerith",
    "hopcroft",
    "hopper",
    "iverson",
    "kahan",
    "kahn",
    "karp",
    "kay",
    "kernighan",
    "knuth",
    "kurzweil",
    "lamport",
    "lampson",
    "lecun",
    "lerdorf",
    "liskov",
    "lovelace",
    "matsumoto",
    "mccarthy",
    "metcalfe",
    "micali",
    "milner",
    "minsky",
    "moler",
    "moore",
    "naur",
    "neumann",
    "newell",
    "nygaard",
    "papert",
    "parnas",
    "pascal",
    "patterson",
    "pearl",
    "perlis",
    "pike",
    "pnueli",
    "rabin",
    "reddy",
    "ritchie",
    "rivest",
    "rossum",
    "russell",
    "scott",
    "sedgewick",
    "shamir",
    "shannon",
    "sifakis",
    "simon",
    "stallman",
    "stearns",
    "steele",
    "stonebraker",
    "stroustrup",
    "sutherland",
    "sutton",
    "tarjan",
    "thacker",
    "thompson",
    "torvalds",
    "turing",
    "ullman",
    "valiant",
    "wadler",
    "wall",
    "wigderson",
    "wilkes",
    "wilkinson",
    "wirth",
    "wozniak",
    "yao",
];

/// Oracle list sizes, pinned so a regenerated table cannot silently shrink.
const ADJECTIVE_COUNT: usize = 219;
/// See [`ADJECTIVE_COUNT`].
const VERB_COUNT: usize = 109;
/// See [`ADJECTIVE_COUNT`].
const NOUN_COUNT: usize = 409;

/// `le = 10` (`src_160477403.js`) — how many times `getPlanSlug` re-rolls a slug
/// that collides with an existing plan file before giving up and keeping the
/// last one.
pub const SLUG_COLLISION_ATTEMPTS: usize = 10;

/// `ynt`'s defaults: at most 4 words, 40 characters.
const SEED_WORDS: usize = 4;
const SEED_MAX_LEN: usize = 40;

/// `g(e)` — a uniform-ish index into a wordlist.
fn random_index(len: usize) -> usize {
    let uuid = uuid::Uuid::new_v4();
    let bytes = uuid.as_bytes();
    let raw = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    (raw as usize) % len
}

fn pick(words: &[&'static str]) -> &'static str {
    words[random_index(words.len())]
}

/// `B7t()` — `${adjective}-${verb}-${noun}`.
#[must_use]
pub fn random_slug() -> String {
    format!("{}-{}-{}", pick(ADJECTIVES), pick(VERBS), pick(NOUNS))
}

/// `HB()` — `${adjective}-${noun}`, the suffix a seeded slug gets.
#[must_use]
pub fn random_suffix() -> String {
    format!("{}-{}", pick(ADJECTIVES), pick(NOUNS))
}

/// `ynt(e, {words:4, maxLen:40})`:
///
/// ```js
/// e.replace(c," ").split(/\s+/).filter(Boolean).slice(0,i).join(" ")
///  .toLowerCase().replace(/[^a-z0-9]+/g,"-").slice(0,r).replace(/^-+|-+$/g,"")
/// ```
///
/// `c` strips the `[Pasted text #1]` / `[Image #2]` attachment markers first.
#[must_use]
pub fn slugify_seed(seed: &str) -> String {
    let stripped = strip_attachment_markers(seed);
    let joined = stripped
        .split_whitespace()
        .take(SEED_WORDS)
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    // `[^a-z0-9]+` → "-", collapsing runs.
    let mut dashed = String::with_capacity(joined.len());
    let mut in_run = false;
    for ch in joined.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            dashed.push(ch);
            in_run = false;
        } else if !in_run {
            dashed.push('-');
            in_run = true;
        }
    }
    // `.slice(0, 40)` is a UTF-16 slice; `dashed` is ASCII by construction here,
    // so a byte slice is the same cut.
    let capped = &dashed[..dashed.len().min(SEED_MAX_LEN)];
    capped.trim_matches('-').to_string()
}

/// The `c` regex in `ynt` — attachment markers are replaced with a space before
/// the word split.
fn strip_attachment_markers(seed: &str) -> String {
    let mut out = String::with_capacity(seed.len());
    let mut rest = seed;
    while let Some(open) = rest.find('[') {
        let (before, tail) = rest.split_at(open);
        let Some(close) = tail.find(']') else {
            out.push_str(before);
            out.push_str(tail);
            return out;
        };
        let inner = &tail[1..close];
        out.push_str(before);
        if is_attachment_marker(inner) {
            out.push(' ');
        } else {
            out.push('[');
            out.push_str(inner);
            out.push(']');
        }
        rest = &tail[close + 1..];
    }
    out.push_str(rest);
    out
}

fn is_attachment_marker(inner: &str) -> bool {
    for prefix in ["Pasted text #", "Image #", "Audio #"] {
        if let Some(tail) = inner.strip_prefix(prefix) {
            return tail.chars().next().is_some_and(|c| c.is_ascii_digit());
        }
    }
    inner.starts_with("...Truncated text #") && inner.ends_with("...")
}

/// `F(r)` — the two files a slug reserves in the plans directory.
#[must_use]
pub fn slug_files(slug: &str) -> [String; 2] {
    [format!("{slug}.md"), format!("{slug}.workshop.md")]
}

/// Whether either file a slug reserves already exists — the port's stand-in for
/// upstream's primed directory listing. ONE definition, so the desktop and
/// mobile hosts cannot disagree about what "taken" means.
#[must_use]
pub fn slug_taken_in(plans_dir: &std::path::Path, slug: &str) -> bool {
    slug_files(slug)
        .iter()
        .any(|name| plans_dir.join(name).exists())
}

/// `getPlanSlug(sessionId, seed)` minus the per-session cache — one slug, with
/// collisions resolved against `taken`.
///
/// `taken` is asked whether a candidate's files already exist; upstream consults
/// a primed listing of the plans directory (`F(r)` = `[${r}.md, ${r}.workshop.md]`).
#[must_use]
pub fn generate_slug(seed: Option<&str>, taken: &dyn Fn(&str) -> bool) -> String {
    let stem = seed.map(slugify_seed).filter(|s| !s.is_empty());
    let mut candidate = String::new();
    for _ in 0..SLUG_COLLISION_ATTEMPTS {
        candidate = match stem.as_deref() {
            Some(stem) => format!("{stem}-{}", random_suffix()),
            None => random_slug(),
        };
        if !taken(&candidate) {
            return candidate;
        }
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wordlists_are_the_oracle_sizes() {
        assert_eq!(ADJECTIVES.len(), ADJECTIVE_COUNT);
        assert_eq!(VERBS.len(), VERB_COUNT);
        assert_eq!(NOUNS.len(), NOUN_COUNT);
    }

    #[test]
    fn a_random_slug_is_three_lowercase_words() {
        let slug = random_slug();
        let parts: Vec<_> = slug.split('-').collect();
        assert_eq!(parts.len(), 3, "{slug}");
        assert!(ADJECTIVES.contains(&parts[0]), "{slug}");
        assert!(VERBS.contains(&parts[1]), "{slug}");
        assert!(NOUNS.contains(&parts[2]), "{slug}");
    }

    #[test]
    fn slugify_seed_follows_ynt() {
        assert_eq!(
            slugify_seed("Fix the login bug now please"),
            "fix-the-login-bug"
        );
        assert_eq!(slugify_seed("  Hello,   World!  "), "hello-world");
        assert_eq!(slugify_seed("!!!"), "");
        // Attachment markers become a SPACE and are then dropped by
        // `filter(Boolean)`, so they do not consume one of the four words.
        assert_eq!(
            slugify_seed("[Pasted text #1] migrate the call sites"),
            "migrate-the-call-sites"
        );
        // A bracketed run that is NOT an attachment marker is ordinary text.
        assert_eq!(slugify_seed("[wip] fix the thing"), "wip-fix-the-thing");
        // A 4-word seed that exceeds 40 chars is cut, then de-dashed.
        assert_eq!(
            slugify_seed("aaaaaaaaaa bbbbbbbbbb cccccccccc dddddddddd"),
            "aaaaaaaaaa-bbbbbbbbbb-cccccccccc-ddddddd"
        );
    }

    #[test]
    fn a_seeded_slug_keeps_the_seed_and_adds_two_words() {
        let slug = generate_slug(Some("ship the port"), &|_| false);
        assert!(slug.starts_with("ship-the-port-"), "{slug}");
        assert_eq!(slug.split('-').count(), 5, "{slug}");
    }

    #[test]
    fn an_empty_seed_falls_back_to_the_three_word_form() {
        let slug = generate_slug(Some("!!!"), &|_| false);
        assert_eq!(slug.split('-').count(), 3, "{slug}");
    }

    #[test]
    fn a_colliding_slug_is_re_rolled() {
        let seen = std::sync::Mutex::new(Vec::new());
        let slug = generate_slug(None, &|candidate| {
            let mut seen = seen.lock().unwrap();
            seen.push(candidate.to_string());
            seen.len() <= 3
        });
        let seen = seen.into_inner().unwrap();
        assert_eq!(seen.len(), 4, "three collisions then an accept");
        assert_eq!(seen.last().unwrap(), &slug);
    }

    #[test]
    fn a_slug_reserves_both_files() {
        assert_eq!(
            slug_files("brave-otter"),
            [
                "brave-otter.md".to_string(),
                "brave-otter.workshop.md".to_string()
            ]
        );
        let dir = std::env::temp_dir().join(format!("lingxi-slug-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!slug_taken_in(&dir, "brave-otter"));
        std::fs::write(dir.join("brave-otter.workshop.md"), "x").unwrap();
        assert!(
            slug_taken_in(&dir, "brave-otter"),
            "the workshop sibling counts"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_exhausted_retry_budget_still_yields_a_slug() {
        let slug = generate_slug(None, &|_| true);
        assert_eq!(slug.split('-').count(), 3, "{slug}");
    }
}
