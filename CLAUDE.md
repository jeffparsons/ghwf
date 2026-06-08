# ghwf development notes

## Adding a config option

When adding a key to `Config` in `src/config.rs`, also:

- offer it in the `ghwf config init` wizard (`src/init.rs`) so interactive
  setup stays complete, and
- document it in the README's annotated `ghwf.toml` example.
