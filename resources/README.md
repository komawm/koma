# Session integration resources

This directory contains [session wrapper](https://github.com/komawm/koma/blob/main/resources/koma-session), [desktop entry](https://github.com/komawm/koma/blob/main/resources/koma.desktop), [systemd user service](https://github.com/komawm/koma/blob/main/resources/koma.service), [shutdown target](https://github.com/komawm/koma/blob/main/resources/koma-shutdown.target), [portal routing](https://github.com/komawm/koma/blob/main/resources/koma-portals.conf), and [greetd example](https://github.com/komawm/koma/blob/main/resources/greetd-config.toml).

The [installer](https://github.com/komawm/koma/blob/main/resources/install-session.sh) writes system integration files using configurable `PREFIX` and `SYSCONFDIR` locations. The wrapper can start/stop session services and change graphical-session environment. Inspect and stage these resources separately after build/runtime validation; do not activate them during workspace initialization or an ordinary nested test.

[jikud.service](https://github.com/komawm/jiku/blob/main/resources/jikud.service) is a proposed companion-service template. The service template now belongs to the separate Jiku repository. No implemented `jikud` binary exists, and Koma's installer does not install this template. Its filesystem restrictions are design inputs requiring dedicated validation once a daemon exists.
