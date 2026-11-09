Zevria is evolving rapidly. **Everything is allowed to break.** Whether the Rust API, event protocol, transcript format, and any other interfaces or data formats may change freely; backward compatibility is not required.

Ensure that all changes preserve the provider’s cacheable prompt prefix.

Always run Cargo commands in release mode (`--release` where supported); commands without build profiles, such as `cargo fmt`, are exempt.
