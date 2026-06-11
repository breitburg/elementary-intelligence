<p align="center">
  <img src="data/icons/128.svg" alt="Beckon icon" width="128" height="128">
</p>

# Beckon

Summon any AI chat service from a system-wide shortcut.

Beckon runs quietly in the background. Press your shortcut
(default <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>Space</kbd>) to bring up a simple
entry, type a message, and press <kbd>Enter</kbd> — the reply streams in
right below the prompt. Press <kbd>Esc</kbd> to clear and dismiss.

Built natively for elementary OS in Rust and GTK4, inheriting the system
stylesheet so it feels at home.

## Features

- **System-wide hotkey** — configurable trigger combination.
- **Spotlight-style entry** — a minimal, centered prompt; the conversation
  slides open beneath it as the answer streams in, rendered as markdown.
- **Any OpenAI-compatible API** — point it at OpenAI, a local server, or any
  compatible endpoint via base URL, key and model.
- **Screenshot to chat** — a second hotkey
  (default <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>S</kbd>) captures the screen and
  attaches it to your message for a vision model to read.
- **Start on login** — runs as a background service.

## How the hotkey works

Wayland has no in-process global key grab, and Pantheon ships no GlobalShortcuts
portal. Instead the app registers a *custom keybinding* with the compositor via
`org.gnome.settings-daemon.plugins.media-keys` (which elementary's
settings-daemon honours). When you press the combo, Gala runs the app with
`--spotlight`, and the already-running single instance shows the entry.

## Build & run

```sh
cargo run            # builds and launches the background service + settings
```

Or install system-wide with meson:

```sh
meson setup build
ninja -C build
sudo ninja -C build install
```

## Configuration

Settings live in
`~/.config/com.github.breitburg.beckon/config.toml` and are also editable from
the app's settings window. Point it at any OpenAI-compatible endpoint:

```toml
api_base_url = "https://api.openai.com/v1"
api_key = "sk-..."
model = "gpt-4o-mini"
system_prompt = "You're a helpful assistant called El. You aim to respond in 1-2 sentences, straight to the point."
shortcut = "<Control><Shift>space"
screenshot_shortcut = "<Control><Shift>s"
```

The file holds your API key, so it's written with `0600` permissions.

## Flatpak & AppCenter

`flatpak/com.github.breitburg.beckon.yml` targets the elementary runtime
(`io.elementary.Platform`) and is what elementary's AppCenter builds. It needs
dconf permissions to register the host keybinding, and vendored cargo sources
for the offline build. Regenerate those whenever `Cargo.lock` changes:

```sh
build-aux/gen-cargo-sources.sh   # writes flatpak/cargo-sources.json
```

Publishing a tagged release also triggers
`.github/workflows/release.yml`, which builds `.flatpak` bundles for x86_64 and
aarch64 and attaches them to the release.

## License

GPL-3.0-or-later
