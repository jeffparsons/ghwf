# #141 — Inline display of attachments that GitHub supports

## Goal

Today every attachment except images renders as a plain link, and the MP4 case is
actively broken (the link points at GitHub's blob *page*, which "seems confused" and
won't play the video). Make `--attach` render each file inline using whatever GitHub
genuinely supports, and fix the broken-MP4 link.

## What GitHub actually supports (verified, not assumed)

I rendered candidate markup through GitHub's own `POST /markdown` (`mode: gfm`) API,
which applies the exact sanitizer GitHub uses on comments. Findings:

- **`<video controls src="…">` survives** sanitization — including inside a `- ` list
  bullet (the trailer format). So committed videos can embed inline via an HTML
  `<video>` tag. `controls`, `muted`, `width`, and `src` attributes are preserved.
- **`src` on a `<source>` child is stripped** — so the URL must live on the `<video>`
  element directly, not on a `<source>` child.
- **`poster` is stripped** — no thumbnail attribute available.
- **`<audio>` is stripped entirely** — GitHub does *not* allow author-written audio
  embeds. Audio therefore cannot be embedded inline and must stay a plain link.
- **Images** continue to embed via `![](…/blob/…?raw=true)` (camo-proxied), unchanged.

This narrows the scope I floated on the issue: it's **images (existing) + video**, not
audio. Audio joins "everything else" as a link. (Whether anything more is possible for
private repos — and a separate thread on whether `<video>` works on private repos at
all, since it isn't camo-proxied — is spun out to #143.)

### Supported video extensions

`mp4`, `mov`, `webm` — the formats GitHub's UI accepts for video attachments and that
browsers play. (`mov` = `video/quicktime`; broadly playable, and what the reporter's
file was.)

## URL forms

- **Images** keep the existing blob link: `https://github.com/{owner}/{repo}/blob/{BRANCH}/{path}`
  with `?raw=true` appended. Camo proxies it; works on public repos.
- **Video** uses the direct raw host as the `<video src>`:
  `https://raw.githubusercontent.com/{owner}/{repo}/{BRANCH}/{path}`. The browser fetches
  it directly (not via camo), and that host serves the right `Content-Type` and
  `Accept-Ranges: bytes` so the player can seek. This is the concrete fix for the broken
  MP4: never hand GitHub a blob-*page* URL for media again.
- **Private repos**: every kind falls back to a plain link to the blob URL, exactly as
  today. Inline embeds depend on anonymous/raw fetches that private repos auth-gate; that
  limitation is unchanged and tracked in #143.

## Implementation (`src/attach.rs`)

All changes are local to `attach.rs`; the call site in `main.rs` and the
`create-issue-comment` / `hand-off` wiring stay as-is.

1. **Classify media kind.** Replace the binary `is_image` with:

   ```rust
   enum MediaKind { Image, Video, Other }

   const IMAGE_EXTS: &[&str] = &[/* unchanged */];
   const VIDEO_EXTS: &[&str] = &["mp4", "mov", "webm"];

   fn media_kind(name: &str) -> MediaKind { /* lowercase ext → Image | Video | Other */ }
   ```

   Audio extensions are deliberately *not* listed — they resolve to `Other` (link),
   because GitHub strips `<audio>`.

2. **Build both URL forms** in `upload()`'s per-file loop: the existing blob URL plus a
   `raw_url` helper:

   ```rust
   fn raw_url(owner, repo, repo_path) -> String  // https://raw.githubusercontent.com/{owner}/{repo}/{BRANCH}/{path}
   ```

3. **Emit markup per kind.** Replace `attachment_markdown` with one that takes the kind,
   both URLs, and `private`:

   - `Image` + public → `![{name}]({blob_url}?raw=true)` (unchanged)
   - `Video` + public → `<video controls src="{raw_url}"></video>`
   - anything else (Other, audio, any kind on a private repo) → `[{name}]({blob_url})`

   `build_trailer` is unchanged — it just bullets whatever lines it's given, and the
   verified test confirms a `<video>` bullet renders.

## Tests (in-module, no network)

- `media_kind` classifies image / video / other extensions, case-insensitively
  (`.MP4`, `.MoV`, …), and treats audio (`mp3`, `wav`) and unknown extensions as `Other`.
- `attachment_markdown`:
  - public image → `![…](…?raw=true)`
  - public video → `<video controls src="https://raw.githubusercontent.com/…">…</video>`
    with the raw host (asserts we did *not* use the blob-page URL)
  - public audio (`.mp3`) → plain link (regression guard: audio is never embedded)
  - private repo, every kind → plain link
- Keep `repo_path` / `sanitize_filename` / `trailer` tests as-is; update the existing
  `markdown_embeds_only_public_images` / `is_image_matches_known_extensions` tests to the
  new `media_kind` / signature.

## Docs

- **README** (lines ~62–70): update the "Images on a **public** repo embed inline …"
  paragraph to "Images and videos on a **public** repo embed inline; audio and other
  files, and everything on a **private** repo, render as links," and keep the private-repo
  caveat.
- **Module doc comment** at the top of `attach.rs`: refresh to describe the
  image-vs-video-vs-link behaviour instead of "images … everything else is a plain link."

No config option is involved (behaviour is automatic by extension), so the
"Adding a config option" checklist in `CLAUDE.md` doesn't apply.

## Verification before finishing

- `cargo test` (unit tests above) and `cargo clippy`.
- Re-run the `POST /markdown` sanitizer check on the *exact* trailer string the new code
  produces, to confirm the `<video>` survives as written.
- If feasible, post a real `--attach clip.mp4` comment to a scratch issue on this (public)
  repo and confirm the player renders and plays. If the live render diverges from the
  `/markdown` API result, fall back to a plain raw-URL link (still fixes the broken MP4)
  and flag it.

## Out of scope

- Private-repo inline images/video — researched in **#143**.
- Audio inline embeds — not possible on GitHub (sanitizer strips `<audio>`); folded into
  the same research ticket as a side note.
