//! App policy data observed in the hash-pinned Claude Code 2.1.310 oracle.
use super::permission_model::AppTier;

const BROWSER_IDS: &[&str] = &[
    "com.apple.Safari",
    "com.apple.SafariTechnologyPreview",
    "com.google.Chrome",
    "com.google.Chrome.beta",
    "com.google.Chrome.dev",
    "com.google.Chrome.canary",
    "com.microsoft.edgemac",
    "com.microsoft.edgemac.Beta",
    "com.microsoft.edgemac.Dev",
    "com.microsoft.edgemac.Canary",
    "org.mozilla.firefox",
    "org.mozilla.firefoxdeveloperedition",
    "org.mozilla.nightly",
    "app.zen-browser.zen",
    "io.gitlab.librewolf-community.librewolf",
    "org.waterfoxproject.waterfox",
    "net.mullvad.mullvadbrowser",
    "org.mozilla.floorp",
    "org.chromium.Thorium",
    "org.chromium.Chromium",
    "com.brave.Browser",
    "com.brave.Browser.beta",
    "com.brave.Browser.nightly",
    "com.operasoftware.Opera",
    "com.operasoftware.OperaGX",
    "com.operasoftware.OperaDeveloper",
    "com.vivaldi.Vivaldi",
    "company.thebrowser.Browser",
    "company.thebrowser.dia",
    "org.torproject.torbrowser",
    "com.duckduckgo.macos.browser",
    "ru.yandex.desktop.yandex-browser",
    "ai.perplexity.comet",
    "com.openai.atlas",
    "com.sigmaos.sigmaos.macos",
    "com.kagi.kagimacOS",
];

const TERMINAL_IDS: &[&str] = &[
    "com.apple.Terminal",
    "com.googlecode.iterm2",
    "dev.warp.Warp-Stable",
    "dev.warp.Warp-Beta",
    "com.github.wez.wezterm",
    "org.alacritty",
    "io.alacritty",
    "net.kovidgoyal.kitty",
    "co.zeit.hyper",
    "com.mitchellh.ghostty",
    "org.tabby",
    "com.termius-dmg.mac",
    "com.microsoft.VSCode",
    "com.microsoft.VSCodeInsiders",
    "com.vscodium",
    "com.todesktop.230313mzl4w4u92",
    "com.exafunction.windsurf",
    "dev.zed.Zed",
    "dev.zed.Zed-Preview",
    "com.jetbrains.intellij",
    "com.jetbrains.intellij.ce",
    "com.jetbrains.pycharm",
    "com.jetbrains.pycharm.ce",
    "com.jetbrains.WebStorm",
    "com.jetbrains.CLion",
    "com.jetbrains.goland",
    "com.jetbrains.rubymine",
    "com.jetbrains.PhpStorm",
    "com.jetbrains.datagrip",
    "com.jetbrains.rider",
    "com.jetbrains.AppCode",
    "com.jetbrains.rustrover",
    "com.jetbrains.fleet",
    "com.google.android.studio",
    "com.axosoft.gitkraken",
    "com.sublimetext.4",
    "com.sublimetext.3",
    "org.vim.MacVim",
    "com.neovim.neovim",
    "org.gnu.Emacs",
    "com.apple.dt.Xcode",
    "org.eclipse.platform.ide",
    "org.netbeans.ide",
    "com.microsoft.visual-studio",
    "com.apple.ScriptEditor2",
    "com.apple.Automator",
    "com.apple.shortcuts",
];

const TRADING_IDS: &[&str] = &[
    "com.webull.desktop.v1",
    "com.webull.trade.mac.v1",
    "com.tastytrade.desktop",
    "com.tradingview.tradingviewapp.desktop",
    "com.fidelity.activetrader",
    "com.fmr.activetrader",
    "com.install4j.5889-6375-8446-2021",
    "com.binance.BinanceDesktop",
    "com.electron.exodus",
    "org.pythonmac.unspecified.Electrum",
    "com.ledger.live",
    "io.trezor.TrezorSuite",
];

const BLOCKED_IDS: &[&str] = &[
    "com.apple.TV",
    "com.apple.Music",
    "com.apple.iBooksX",
    "com.apple.podcasts",
    "com.spotify.client",
    "com.amazon.music",
    "com.tidal.desktop",
    "com.deezer.deezer-desktop",
    "com.pandora.desktop",
    "com.electron.pocket-casts",
    "au.com.shiftyjelly.PocketCasts",
    "tv.plex.desktop",
    "tv.plex.htpc",
    "tv.plex.plexamp",
    "com.amazon.aiv.AIVApp",
    "net.kovidgoyal.calibre",
    "com.amazon.Kindle",
    "com.amazon.Lassen",
    "com.kobo.desktop.Kobo",
    "org.qbittorrent.qBittorrent",
    "org.m0k.transmission",
    "org.deluge",
    "com.biglybt",
    "io.webtorrent.webtorrent",
    "com.bittorrent.uTorrent",
    "com.bitTorrent.utweb",
    "com.bittorrent.BitTorrent",
    "com.frostwire.FrostWire",
];

const BLOCKED_NAMES: &[&str] = &[
    "netflix",
    "disney+",
    "hulu",
    "prime video",
    "apple tv",
    "peacock",
    "paramount+",
    "tubi",
    "crunchyroll",
    "vudu",
    "kindle",
    "apple books",
    "kobo",
    "play books",
    "calibre",
    "libby",
    "readium",
    "audible",
    "libro.fm",
    "speechify",
    "spotify",
    "apple music",
    "amazon music",
    "youtube music",
    "tidal",
    "deezer",
    "pandora",
    "pocket casts",
    "torrent",
    "transmission",
    "deluge",
    "biglybt",
    "tixati",
    "frostwire",
    "naver",
    "reddit",
    "sony music",
    "vegas pro",
    "pitchfork",
    "economist",
    "nytimes",
];

const BLOCKED_EXES: &[&str] = &["plex.exe", "plexamp.exe", "plex htpc.exe"];

const BROWSER_WINDOWS_IDS: &[&str] = &[
    "Microsoft.MicrosoftEdge.Stable_8wekyb3d8bbwe!MSEDGE",
    "Microsoft.MicrosoftEdge.Beta_8wekyb3d8bbwe!MSEDGE",
    "Microsoft.MicrosoftEdge.Dev_8wekyb3d8bbwe!MSEDGE",
    "Microsoft.MicrosoftEdge.Canary_8wekyb3d8bbwe!MSEDGE",
    "Mozilla.Firefox_n80bbvh6b1yt2!App",
    "TheBrowserCompany.Arc_ttt1ap7aakyb4!App",
];

const TERMINAL_WINDOWS_IDS: &[&str] = &[
    "Microsoft.WindowsTerminal_8wekyb3d8bbwe!App",
    "Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe!App",
    "Microsoft.PowerShell_8wekyb3d8bbwe!App",
];

const BROWSER_EXES: &[&str] = &[
    "chrome.exe",
    "msedge.exe",
    "firefox.exe",
    "brave.exe",
    "opera.exe",
    "vivaldi.exe",
    "chromium.exe",
    "arc.exe",
    "duckduckgo.exe",
    "zen.exe",
    "librewolf.exe",
    "waterfox.exe",
    "mullvadbrowser.exe",
    "floorp.exe",
    "comet.exe",
];

const TERMINAL_EXES: &[&str] = &[
    "cmd.exe",
    "powershell.exe",
    "pwsh.exe",
    "wt.exe",
    "conemu.exe",
    "conemu64.exe",
    "cmder.exe",
    "alacritty.exe",
    "wezterm-gui.exe",
    "warp.exe",
    "hyper.exe",
    "zed.exe",
    "tabby.exe",
    "termius.exe",
    "code.exe",
    "code - insiders.exe",
    "cursor.exe",
    "vscodium.exe",
    "windsurf.exe",
    "sublime_text.exe",
    "devenv.exe",
    "gitkraken.exe",
    "idea64.exe",
    "pycharm64.exe",
    "webstorm64.exe",
    "goland64.exe",
    "clion64.exe",
    "rider64.exe",
];

const SHELL_EXES: &[&str] = &[
    "explorer.exe",
    "searchhost.exe",
    "searchapp.exe",
    "searchui.exe",
    "startmenuexperiencehost.exe",
    "shellexperiencehost.exe",
    "taskmgr.exe",
];

const TRADING_EXES: &[&str] = &[
    "webull.exe",
    "tradingview.exe",
    "tws.exe",
    "thinkorswim.exe",
    "binance.exe",
    "ledger live.exe",
    "trezor suite.exe",
];

const BROWSER_NAMES: &[&str] = &[
    "safari",
    "chrome",
    "firefox",
    "microsoft edge",
    "brave",
    "opera",
    "vivaldi",
    "chromium",
    "arc browser",
    "tor browser",
    "duckduckgo",
    "yandex",
    "orion browser",
    "zen browser",
    "librewolf",
    "waterfox",
    "mullvad browser",
    "floorp",
    "comet",
    "atlas browser",
    "sigmaos",
    "dia browser",
];

const TERMINAL_NAMES: &[&str] = &[
    "terminal",
    "iterm",
    "wezterm",
    "alacritty",
    "kitty",
    "ghostty",
    "tabby",
    "termius",
    "script editor",
    "automator",
    "powershell",
    "cmd.exe",
    "command prompt",
    "git bash",
    "conemu",
    "cmder",
    "visual studio code",
    "visual studio",
    "vscode",
    "vs code",
    "vscodium",
    "cursor",
    "windsurf",
    "intellij",
    "pycharm",
    "webstorm",
    "clion",
    "goland",
    "rubymine",
    "phpstorm",
    "datagrip",
    "rider",
    "appcode",
    "rustrover",
    "fleet",
    "android studio",
    "sublime text",
    "macvim",
    "neovim",
    "emacs",
    "xcode",
    "eclipse",
    "netbeans",
];

const TRADING_NAMES: &[&str] = &[
    "bloomberg",
    "ameritrade",
    "thinkorswim",
    "schwab",
    "fidelity",
    "e*trade",
    "interactive brokers",
    "trader workstation",
    "tradestation",
    "webull",
    "robinhood",
    "tastytrade",
    "ninjatrader",
    "tradingview",
    "moomoo",
    "tradezero",
    "prorealtime",
    "plus500",
    "saxotrader",
    "oanda",
    "metatrader",
    "forex.com",
    "avaoptions",
    "ctrader",
    "jforex",
    "iq option",
    "olymp trade",
    "binomo",
    "pocket option",
    "raceoption",
    "expertoption",
    "quotex",
    "naga",
    "morgan stanley",
    "ubs neo",
    "eikon",
    "coinbase",
    "kraken",
    "binance",
    "okx",
    "bybit",
    "phemex",
    "stormgain",
    "crypto.com",
    "electrum",
    "ledger live",
    "trezor",
    "guarda",
    "atomic wallet",
    "bitpay",
    "bisq",
    "koinly",
    "cointracker",
    "blockfi",
    "stripe cli",
    "decentraland",
    "axie infinity",
    "gods unchained",
];

fn executable(id: &str) -> String {
    id.rsplit(['/', '\\']).next().unwrap_or("").to_lowercase()
}

pub fn category(id: Option<&str>, name: &str) -> Option<&'static str> {
    if let Some(id) = id {
        if BROWSER_IDS.contains(&id) || BROWSER_WINDOWS_IDS.contains(&id) {
            return Some("browser");
        }
        if TERMINAL_IDS.contains(&id) || TERMINAL_WINDOWS_IDS.contains(&id) {
            return Some("terminal");
        }
        if TRADING_IDS.contains(&id) {
            return Some("trading");
        }
        let exe = executable(id);
        if BROWSER_EXES.contains(&exe.as_str()) {
            return Some("browser");
        }
        if TERMINAL_EXES.contains(&exe.as_str()) {
            return Some("terminal");
        }
        if TRADING_EXES.contains(&exe.as_str()) {
            return Some("trading");
        }
        if SHELL_EXES.contains(&exe.as_str()) {
            return Some("shell");
        }
    }
    let name = name.to_lowercase();
    for (words, category) in [
        (TRADING_NAMES, "trading"),
        (BROWSER_NAMES, "browser"),
        (TERMINAL_NAMES, "terminal"),
    ] {
        if words.iter().any(|word| name.contains(word)) {
            return Some(category);
        }
    }
    None
}

pub fn maximum_tier(id: Option<&str>, name: &str) -> AppTier {
    match category(id, name) {
        Some("browser" | "trading") => AppTier::Read,
        Some("terminal" | "shell") => AppTier::Click,
        _ => AppTier::Full,
    }
}

pub fn blocked(id: Option<&str>, name: &str) -> bool {
    if let Some(id) = id {
        if id.starts_with('<')
            || BLOCKED_IDS.contains(&id)
            || BLOCKED_EXES.contains(&executable(id).as_str())
        {
            return true;
        }
    }
    let name = name.to_lowercase();
    let compact: String = name
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '\u{feff}')
        .collect();
    BLOCKED_NAMES.iter().any(|word| {
        name.contains(word)
            || word.contains(' ')
                && *word != "play books"
                && compact.contains(&word.replace(' ', ""))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reference_classification_cases() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../fixtures/claude-code-2.1.310.json")).unwrap();
        for case in fixture["classification"].as_array().unwrap() {
            let id = case["input"]["bundleId"].as_str();
            let name = case["input"]["displayName"].as_str().unwrap();
            assert_eq!(
                category(id, name),
                case["output"]["category"].as_str(),
                "{case}"
            );
            assert_eq!(
                maximum_tier(id, name).as_str(),
                case["output"]["tier"].as_str().unwrap(),
                "{case}"
            );
            assert_eq!(
                blocked(id, name),
                case["output"]["policyDenied"].as_bool().unwrap(),
                "{case}"
            );
        }
    }
}
