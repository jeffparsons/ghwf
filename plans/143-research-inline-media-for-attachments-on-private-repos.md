# Plan: Research inline media for attachments on private repos (#143)

## Goal / deliverable

This is a research ticket. The deliverable is a **determination**, backed by an
empirical test, of whether any attachment media can embed inline on **private**
repos — plus whatever small code change that determination justifies, and
documentation so the conclusion isn't re-litigated.

The pre-plan research already settled most threads (see issue comment). The one
thing that can't be settled without a real logged-in browser is whether
`<video>` can be made to play on a private repo by changing the `src` URL form.
Everything else is a documentation/decision exercise.

## Background (established in pre-plan)

- **Images:** dead end. `![]()`/`<img>` always go through camo, which fetches
  anonymously and can't reach private raw bytes. The only thing that inlines on
  private repos is GitHub's own drag-drop `user-attachments/assets/…` flow,
  which mints a short-lived signed URL at page-render time; there's no
  token/PAT upload API into that CDN. No change possible.
- **Video:** GitHub does **not** camo-proxy `<video>`; the viewer's
  authenticated browser fetches the `src` directly. Our current `src` is
  `raw.githubusercontent.com/…`, which a browser can't authenticate to
  (separate origin, no session cookie) → confirmed 404 on private. The untested
  idea is pointing `<video src>` at the **github.com `/blob/<branch>/file?raw=true`**
  (or `/raw/<branch>/file`) URL, which the browser *can* authenticate to via
  the github.com session cookie and may follow to the tokened raw bytes.
- **Audio:** moot — GitHub's sanitiser strips `<audio>` on public and private
  alike. Stays a link regardless.
- **Tokened raw URLs in comments:** rejected (ephemeral + credential leak).

## Phase 1 — Empirical video test (the crux)

Settle, in one browser look, which (if any) `<video src>` URL form plays on a
private repo for an authenticated viewer.

**Setup** (reuse the existing private repo `jeffatstile/ghwf-smoke-test`; no new
repo needed):

1. Generate a tiny self-contained test clip (≈1 s, a few KB), e.g. with
   `ffmpeg` (`testsrc`) if available, otherwise download a small public-domain
   sample mp4 and a small webm. Keep both an `.mp4` and a `.webm` to test codec
   handling.
2. Commit the clip(s) to an orphan `ghwf-attachments`-style branch in the
   private repo using the same Git Data API path ghwf already uses (or a plain
   `git push` of an orphan branch — the branch mechanics aren't what's under
   test, the URL form is). Record the branch name and file paths.
3. Open a throwaway issue in the private repo and post a single comment (via
   `gh`/`ghwf create-issue-comment`) containing each variant, clearly labelled,
   so one screenshot settles all of them:

   - **A (current):** `<video controls src="https://raw.githubusercontent.com/jeffatstile/ghwf-smoke-test/<branch>/<path>.mp4"></video>`
   - **B (blob ?raw=true):** `<video controls src="https://github.com/jeffatstile/ghwf-smoke-test/blob/<branch>/<path>.mp4?raw=true"></video>`
   - **C (/raw/ web path):** `<video controls src="https://github.com/jeffatstile/ghwf-smoke-test/raw/<branch>/<path>.mp4"></video>`
   - **D (webm via B):** same as B with the `.webm` file (in case mp4
     content-type/codec is the blocker but webm isn't).
   - **E (image sanity check, expected broken):** `![img](https://github.com/jeffatstile/ghwf-smoke-test/blob/<branch>/<a-tiny>.png?raw=true)` — to definitively confirm on the record that the image path is dead, not just assumed.

**Verification** (needs the user — I can't see rendered private-repo HTML):
hand off with the comment link and ask the user to report, for each of A–E,
whether it (i) plays inline, (ii) shows a broken player / download prompt, or
(iii) renders nothing. Use `ghwf hand-off --question` (or `ghwf ask` with
checkboxes per variant). Capture their answer verbatim in the findings.

If any variant plays, also confirm it plays for a **second** authenticated user
with repo access if feasible (rules out "works only because I'm the author"),
and note behaviour for a user *without* access (should fail closed).

## Phase 2 — Act on the result

**If a variant plays (most likely B or C):**

- Update `src/attach.rs` `attachment_markdown()` so private-repo video emits the
  working `src` form instead of falling back to a plain link. Concretely, drop
  the `!private` guard on `MediaKind::Video` and switch the video `src` to the
  winning URL (likely the `blob/<branch>/…?raw=true` or `/raw/<branch>/…` form).
  Consider unifying public video to the same form so there's one code path.
- Keep images and audio on the link fallback (unchanged).
- Add/adjust unit tests in `attach.rs` for the markdown emitted per
  `(MediaKind, private)` combination to lock in the new behaviour.

**If nothing plays:**

- Leave the code as-is (link fallback for all private media).
- Record the negative result so it's not re-investigated.

## Phase 3 — Documentation (regardless of outcome)

- Update the comments at the top of `src/attach.rs` and on
  `github::repo_is_private` to reflect the verified facts (camo-anonymous for
  images; video `src` mechanics and the verified outcome).
- Update the README's attachments section to state plainly what does and
  doesn't embed inline on private repos.
- Post a final summary comment on #143 with the empirical results and the
  decision, and close out the research.

## Out of scope / follow-ups

- **Blob-SHA pinning** for attachment URLs (durability if the
  `ghwf-attachments` branch is ever rewritten) — orthogonal robustness tweak;
  file as a follow-up issue if we think it's worth it, don't do it here.
- Any attempt to integrate with GitHub's `user-attachments/assets/…` CDN —
  no token-auth upload path exists; not pursued.
- Downscaled/preview images — still camo-blocked; not pursued.

## Testing

- `cargo test` (unit tests in `attach.rs` for the markdown matrix).
- `cargo fmt` / `cargo clippy` clean.
- The decisive functional check is the manual browser observation in Phase 1
  (recorded in the issue), since inline media rendering can't be unit-tested.

## Files likely touched

- `src/attach.rs` — video markdown (if a variant plays) + tests + doc comments.
- `src/github.rs` — doc comment on `repo_is_private`.
- `README.md` — attachments behaviour on private repos.
- The private-repo test artifacts are throwaway (test branch + throwaway issue),
  not committed to this repo.
