-- Koma Dotfiles Showcase — Hybrid Physics, Gradient Borders & Event Closures
-- Demonstrates:
--   1. Hooke semi-implicit Euler springs (`koma.spring`) & custom cubic bezier (`koma.bezier`)
--   2. Dual-stop rotating/angled border gradients & Dual Kawase frosted blur
--   3. Extended per-window rice rules (`inactive_opacity`, `animation`, `no_blur`, `no_shadow`, `border_color`)
--   4. Read-only desktop queries + safe Ring 0 dispatch inside Lua keybind closures (`ctx:windows()`, `ctx:dispatch(...)`)
--   5. Event-driven compositor automation via `koma.on("output.changed", ...)`

-- 1. Register named physics springs & bezier curves
koma.bezier("overshoot_pop", 0.05, 0.9, 0.1, 1.08, 240)
koma.bezier("silk_glide", 0.22, 1.0, 0.36, 1.0, 260)
koma.spring("bouncy_spring", { stiffness = 420.0, damping = 18.0, mass = 1.0, epsilon = 0.0005 })

-- 2. Monitors
koma.monitor({ output = "eDP-1", mode = "2560x1600@240", scale = 1.5, vrr = "on_demand" })
koma.monitor({ output = "DP-1", mode = "3440x1440@165", scale = 1.0, vrr = "always" })

-- 3. Appearance: dual-stop rotating border gradient, Dual Kawase blur, hybrid animations
koma.appearance({
    corner_radius = 12,
    border = {
        enabled = true,
        width = 2.0,
        active_color = "#7aa2f7",
        active_color_end = "#bb9af7",
        inactive_color = "#292e42",
        inactive_color_end = "#1f2335",
        agent_leased_color = "#9ece6a",
        human_preempt_color = "#ff9e64",
        gradient_angle = 45.0,
        gradient_rotate_rpm = 12.0,
    },
    blur = {
        enabled = true,
        size = 7,
        passes = 3,
        noise = 0.0117,
        contrast = 0.92,
        brightness = 0.85,
        vibrancy = 0.20,
        xray = true,
        ignore_opacity = false,
    },
    animations = {
        enabled = true,
        solver = "spring",
        stiffness = 780.0,
        damping_ratio = 0.88,
        open_scale_from = 0.86,
        close_scale_to = 0.92,
        windows_open = { curve = "overshoot_pop", style = "popin" },
        windows_close = { curve = "silk_glide", style = "slide:bottom" },
        windows_move = { curve = "bouncy_spring" },
        workspaces = { curve = "silk_glide", style = "slide" },
        border = { duration_ms = 180 },
    },
})

-- 4. Extended per-window rice rules
koma.rule.window({
    name = "glass-terminal-rice",
    matches = { { app_id = "^(com\\.mitchellh\\.ghostty|org\\.wezfurlong\\.wezterm|foot)$" } },
    opacity = 0.92,
    inactive_opacity = 0.82,
    corner_radius = 14,
    animation = "popin",
    border_color = "#7dcfff",
})

koma.rule.window({
    name = "clean-media-fullscreen",
    matches = { { app_id = "^(mpv|imv)$" } },
    no_blur = true,
    no_shadow = true,
    animation = "slide:top",
})

koma.rule.window({
    name = "vault-redacted",
    matches = { { app_id = "^(org\\.keepassxc\\.KeePassXC|bitwarden)$" } },
    block_out_from = "all",
    no_shadow = true,
})

-- 5. Dynamic "Run-or-Raise" terminal keybind using read-only desktop queries + Ring 0 dispatch
koma.bind("Mod + Return", function(ctx)
    for _, win in ipairs(ctx:windows()) do
        if win.app_id and string.find(win.app_id, "ghostty") then
            ctx:dispatch(koma.act.window.focus(win.id))
            return
        end
    end
    ctx:spawn({ "ghostty" })
end, { desc = "Run-or-Raise Ghostty terminal" })

-- 6. Ultrawide auto-layout hook: switch focused workspace to Scrolling on > 2560px displays
koma.on("output.changed", function(ctx, ev)
    if ev.width and ev.width >= 3440 then
        ctx:dispatch(koma.act.workspace.set_layout("scrolling"))
    end
end)
