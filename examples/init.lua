---@diagnostic disable: undefined-global
-- Generate IDE autocomplete stubs via: komactl gen-types

-- 1. Modular user imports jailed under ~/.config/koma/lua/
-- require("macros.wps")
-- require("macros.blender")

-- 2. Session Environment & Autostart Daemons
koma.env("XCURSOR_SIZE", "24")
koma.env("QT_QPA_PLATFORM", "wayland;xcb")

koma.on("compositor.start", function(ctx)
    ctx:spawn({ "swww-daemon" })
    ctx:spawn({ "waybar" })
    ctx:spawn({ "fcitx5", "-d" })
    ctx:spawn({ "koma-overview", "--daemon" })
end)

-- 3. Physical Monitor & Symmetrical Workspaces
koma.monitor({
    output = "eDP-1",
    mode = "2560x1600@240.000",
    scale = 1.6,
    vrr = "on_demand",
    focus_at_startup = true,
})

-- Workspaces 1..4 inherit the global default `dwindle` binary tree; Workspace 5 opts into Niri `scrolling`!
koma.layout("dwindle")

for i = 1, 4 do
    koma.workspace({
        name = tostring(i),
        presentation = "headed",
        preferred_output = "eDP-1",
        layout = "dwindle",
    })
end

koma.workspace({
    name = "5",
    presentation = "headed",
    preferred_output = "eDP-1",
    layout = "scrolling",
})

koma.workspace({
    name = "special:magic",
    presentation = "headed",
    preferred_output = "eDP-1",
    layout = "dwindle",
    gaps = { inner = 10, outer = 48 },
})

koma.dwindle({
    force_split = 0,
    preserve_split = true,
    smart_split = false,
    use_active_for_splits = false,
    default_split_ratio = 1.0,
    pseudotile = true,
})

-- Niri-faithful Scrolling strip config (preset column widths, camera spring & 30px edge auto-scroll)
koma.scrolling({
    preset_column_widths = { 1.0 / 3.0, 0.5, 2.0 / 3.0 },
    preset_window_heights = { 1.0 / 3.0, 0.5, 2.0 / 3.0 },
    default_column_width = 0.5,
    default_column_display = "normal",
    tab_indicator = {
        enabled = true,
        hide_when_single_tab = true,
        place_within_column = false,
        position = "left",
        width = 4.0,
        gap = 4.0,
        gaps_between_tabs = 4.0,
        length_proportion = 0.5,
        corner_radius = 2.0,
    },
    center_focused_column = "on_overflow",
    always_center_single_column = true,
    camera_damping_ratio = 1.0,
    camera_stiffness = 800.0,
    swipe_working_area_movement = 1200.0,
    edge_auto_scroll = {
        enabled = true,
        trigger_width = 30.0,
        delay_ms = 100,
        max_speed = 1500.0,
    },
})

koma.workspace({
    name = "agent:work",
    presentation = "headless",
    viewport = { width = 1920, height = 1080, scale = 1.0 },
    layout = "deck",
})

-- 4. Input & Appearance
koma.input({
    mod_key = "Super",
    inherit_default_binds = true,
    touchpad = { tap_to_click = true, natural_scroll = true },
    tablet = { map_to_output = "eDP-1" },
})

koma.appearance({
    corner_radius = 12,
    border = {
        enabled = true,
        width = 2.0,
        active_color = "#58a6ff",
        inactive_color = "#30363d",
        agent_leased_color = "#d2a8ff",
        human_preempt_color = "#f0883e",
    },
    shadow = {
        enabled = true,
        range = 16,
        render_power = 3,
        sharp = false,
        color = "#00000073",
        offset_x = 0.0,
        offset_y = 4.0,
        scale = 1.0,
    },
    dim = {
        dim_inactive = false,
        dim_strength = 0.15,
        brightness = 1.0,
        contrast = 1.0,
    },
    blur = {
        enabled = true,
        size = 6,
        passes = 3,
        noise = 0.0117,
        contrast = 0.8916,
        brightness = 0.8172,
        vibrancy = 0.1696,
        xray = true,
        ignore_opacity = false,
    },
    animations = {
        enabled = true,
        damping_ratio = 1.0,
        stiffness = 800.0,
        open_scale_from = 0.88,
    },
})

-- 5. Core Keybindings (inherits Hyprland default keymap; override or unbind selectively)
-- Example: koma.unbind("Mod + P")
koma.unbind("Mod + P")
koma.bind("Mod + P",         koma.act.window.pseudotile_toggle(), { desc = "Toggle Dwindle pseudotile" })
koma.bind("Mod + Return", koma.act.spawn({ "ghostty" }), { desc = "Open terminal" })
koma.bind("Mod + D",      koma.act.spawn({ "fuzzel" }),  { desc = "Application launcher" })
koma.bind("Mod + Q",         koma.act.window.close(),             { desc = "Close active window" })
koma.bind("Mod + V",         koma.act.window.float_toggle(),      { desc = "Toggle floating layer" })
koma.bind("Mod + E",         koma.act.dwindle.toggle_split(),     { desc = "Toggle Dwindle split axis" })
koma.bind("Mod + R",         koma.act.scrolling.switch_preset_column_width(),      { desc = "Cycle Scrolling column width forward" })
koma.bind("Mod + Ctrl + R",  koma.act.scrolling.switch_preset_column_width_back(), { desc = "Cycle Scrolling column width backward" })
koma.bind("Mod + C",         koma.act.scrolling.center_column(),                   { desc = "Center active Scrolling column" })
koma.bind("Mod + M",                koma.act.scrolling.maximize_column(),                 { desc = "Maximize active Scrolling column" })
koma.bind("Mod + Ctrl + Shift + R", koma.act.scrolling.switch_preset_window_height(),     { desc = "Cycle Scrolling tile height preset" })
koma.bind("Mod + Comma",            koma.act.scrolling.consume_window_into_column(),      { desc = "Consume right window into column stack" })
koma.bind("Mod + Period",           koma.act.scrolling.expel_window_from_column(),        { desc = "Expel focused window out of column stack" })
koma.bind("Mod + BracketLeft",      koma.act.scrolling.consume_or_expel_window_left(),    { desc = "Consume or expel window left" })
koma.bind("Mod + BracketRight",     koma.act.scrolling.consume_or_expel_window_right(),   { desc = "Consume or expel window right" })
koma.bind("Mod + S",         koma.act.scratchpad.toggle("magic"), { desc = "Toggle Scratchpad overlay" })
koma.bind("Mod + Shift + S", koma.act.scratchpad.move_window("magic", true), { desc = "Send/pop Scratchpad" })
koma.bind("Mod + H",         koma.act.focus.left(),               { desc = "Focus left tile" })
koma.bind("Mod + J",         koma.act.focus.down(),               { desc = "Focus down tile" })
koma.bind("Mod + K",         koma.act.focus.up(),                 { desc = "Focus up tile" })
koma.bind("Mod + L",         koma.act.focus.right(),              { desc = "Focus right tile" })
koma.bind("Mod + Shift + H", koma.act.window.move_direction("left"),  { desc = "Move window left" })
koma.bind("Mod + Shift + J", koma.act.window.move_direction("down"),  { desc = "Move window down" })
koma.bind("Mod + Shift + K", koma.act.window.move_direction("up"),    { desc = "Move window up" })
koma.bind("Mod + Shift + L", koma.act.window.move_direction("right"), { desc = "Move window right" })
koma.bind("Mod + Tab",       koma.act.overview.toggle(),          { desc = "Toggle workspace overview" })

for i = 1, 5 do
    local ws = tostring(i)
    koma.bind("Mod + " .. ws,         koma.act.workspace.focus(ws))
    koma.bind("Mod + Shift + " .. ws, koma.act.window.move_to_workspace(ws))
end

-- Toggle peek view onto the background Agent workspace (Mod + G takes over write control)
koma.bind("Mod + A", koma.act.workspace.peek("agent:work"), {
    desc = "Peek at agent:work in Read-Only Shield mode",
})

-- Hyprland/Niri-style Mod + Mouse Interactive Window Move (4-quadrant smart_split) & Split-Ratio Resize
koma.bind("Mod + MouseLeft",  koma.act.window.drag_move(),   { desc = "Drag move floating/tiled window (RMB mid-drag toggles float)" })
koma.bind("Mod + MouseRight", koma.act.window.drag_resize(), { desc = "Drag resize corner / Dwindle split ratio" })

-- Niri-style Mod + Mouse Wheel & Touchpad Two-Finger Scroll Bindings (per-axis consumption + cooldown)
koma.bind("Mod + WheelScrollDown",      koma.act.workspace.focus_next(), { cooldown_ms = 150, desc = "Next workspace" })
koma.bind("Mod + WheelScrollUp",        koma.act.workspace.focus_prev(), { cooldown_ms = 150, desc = "Previous workspace" })
koma.bind("Mod + Shift + WheelScrollDown", koma.act.deck.cycle_next(),   { cooldown_ms = 120, desc = "Next Deck window" })
koma.bind("Mod + Shift + WheelScrollUp",   koma.act.deck.cycle_prev(),   { cooldown_ms = 120, desc = "Previous Deck window" })
koma.bind("Mod + TouchpadScrollRight",  koma.act.deck.cycle_next(),      { cooldown_ms = 120, desc = "Cycle Deck right" })
koma.bind("Mod + TouchpadScrollLeft",   koma.act.deck.cycle_prev(),      { cooldown_ms = 120, desc = "Cycle Deck left" })

-- Multi-Finger Touchpad Gestures (2-finger pinch remains unclaimed for Wayland client zoom!)
koma.gesture({ fingers = 3, direction = "horizontal", action = koma.act.scrolling.swipe(), desc = "3-finger 1:1 Scrolling swipe & velocity projection" })
koma.gesture({ fingers = 3, direction = "vertical",   action = koma.act.scroll_strip.swipe(), desc = "3-finger Workspace switch" })
koma.gesture({ fingers = 4, direction = "up",         action = koma.act.overview.toggle(),    cooldown_ms = 200 })
koma.gesture({ fingers = 4, direction = "pinch_in",   action = function(ctx)
    ctx:spawn({ "koma-overview", "toggle" })
end, cooldown_ms = 250 })

-- Bind a parameterized macro directly to a human shortcut
koma.bind("Ctrl + Alt + H", koma.act.macro.run("wps.drag_first_line_indent", { delta_px = 64 }), {
    desc = "Indent paragraph first line in WPS Writer",
})

-- 6. Baseline Window Rules (Frosted-Glass Terminals & Protected Credential Blackout)
koma.rule.window({
    name = "frosted-glass-terminals",
    matches = { { app_id = "^(com\\.mitchellh\\.ghostty|org\\.wezfurlong\\.wezterm|foot)$" } },
    opacity = 0.90,
    corner_radius = 12,
})

koma.rule.window({
    name = "protect-password-managers",
    matches = { { app_id = "^(org\\.keepassxc\\.KeePassXC|bitwarden)$" } },
    block_out_from = "all",
})


koma.bind("Mod + W", koma.act.scrolling.toggle_column_tabbed_display(), { desc = "Toggle Scrolling column Tabbed / Split mode" })
