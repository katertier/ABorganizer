# ABorganizer

> [!CAUTION]
> ### THIS IS PRE-ALPHA, PRE-PRE-ALPHA SCAFFOLDING. DO NOT USE.
>
> Reading this README does not constitute an invitation to use the
> code. The code does not exist yet in any usable form. What's here
> is a workspace skeleton intended to compile cleanly and pass
> lints, not to do useful work.
>
> **Concrete state today:**
> - The daemon binds two HTTP ports and returns `{ "status": "ok" }`.
>   That's it. It does not scan, tag, transcribe, play, or organize
>   anything.
> - The CLI prints `--help` and tells you commands are not yet
>   implemented.
> - The pipeline stage trait + executor compile. Zero stages are
>   registered. Scanning a directory produces no output.
> - The database schema is committed but no code writes to it.
> - The Swift FFI bridge is a stub.
> - The Audiobookshelf-compat API has two informational endpoints
>   (`/healthcheck`, `/api/info`) and 100+ missing ones.
> - The web UIs are blank-page placeholders.
>
> **If you use this code:**
> - It WILL produce no visible output.
> - It WILL waste disk space holding empty databases.
> - It MAY conflict with port 8429 or 13378 if you run it without
>   checking what else is on your machine.
> - It WILL NOT delete, corrupt, or modify your existing audio files
>   (it doesn't touch them yet).
> - It WILL break in interesting ways the moment any actual feature
>   lands, because every API surface is going to change.
> - It MAY cause earthquakes, the heat death of the universe, your
>   cat developing opinions about jazz, or your audiobook collection
>   spontaneously reorganizing itself into Dewey decimal order.
>   (Probability low. Severity high. Insurance unavailable.)
>
> **Documentation lives privately with the maintainer until the code
> can match it.** What's in this repo is the scaffold. Detailed
> design docs (architecture, ADRs, schema, API, policies, roadmap)
> are kept out of the repo until they describe something that
> actually runs. Don't extrapolate intent from filenames alone.
>
> **What works right now is:**
>
> ```
> git clone https://github.com/katertier/ABorganizer
> cd ABorganizer
> cargo build --workspace
> cargo test --workspace
> cargo xtask check
> ```
>
> All four should complete cleanly. That's the entire user-facing
> surface at this moment.
>
> **Issues are welcome** for design questions, not for "this doesn't
> work" (correct — it doesn't, by design). Pull requests against an
> empty house are also welcome but unlikely to be productive.
>
> **Watch the repo** to know when this warning gets shorter. That
> will be the signal that there's something to look at.

---

AudioBook organizer using Audible / Audnexus and Apple Intelligence
to tag and organize audiobooks on macOS 26+ (Apple Silicon).

**Target audience:** the maintainer, a handful of nerds with the
patience to follow along.

**License:** MIT.
