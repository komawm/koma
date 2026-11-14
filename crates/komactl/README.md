# komactl

Admin IPC, configuration validation/reload, LuaLS generation and sandbox launch support. `validate` and `gen-types` can be used without a compositor after building; live RPC commands need the intended Koma instance.

This is one crate within [Koma](https://github.com/komawm/koma), not a separate Git repository. See the [workspace manifest](https://github.com/komawm/koma/blob/main/Cargo.toml), [architecture](https://github.com/komawm/docs/blob/main/docs/ARCHITECTURE.md), [agent guidance](https://github.com/komawm/koma/blob/main/AGENTS.md), and [testing plan](https://github.com/komawm/docs/blob/main/docs/development/testing.md).
