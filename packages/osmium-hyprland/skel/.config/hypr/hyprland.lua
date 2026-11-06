-- OsmiumOS default Hyprland config (Lua, Hyprland >= 0.57). It is yours: edit freely.
-- Theme: synthwave. Palette: #0b0221 night, #1a1033 dusk, #ff2bd6 neon pink, #00e5ff cyan, #ffb000 sun.

local mainMod  = "SUPER"
local terminal = "alacritty"
local menu     = "anyrun"
local files    = "cosmic-files"

hl.monitor({ output = "", mode = "preferred", position = "auto", scale = "auto" })

hl.env("XCURSOR_SIZE", "24")
hl.env("HYPRCURSOR_SIZE", "24")

hl.on("hyprland.start", function()
    hl.exec_cmd("awww-daemon")
    hl.exec_cmd("sleep 1 && awww img --transition-type grow /usr/share/backgrounds/osmium/synthwave.png")
    hl.exec_cmd("ironbar")
    hl.exec_cmd("swaync")
    hl.exec_cmd("hypridle")
    hl.exec_cmd("systemctl --user start hyprpolkitagent")
    hl.exec_cmd("swayosd-server")
    hl.exec_cmd("osmium-welcome --launch")
end)

hl.config({
    general = {
        gaps_in     = 5,
        gaps_out    = 10,
        border_size = 2,
        col = {
            active_border   = { colors = { "rgba(ff2bd6ff)", "rgba(00e5ffff)" }, angle = 45 },
            inactive_border = "rgba(3a1d5caa)",
        },
        layout = "dwindle",
    },
    decoration = {
        rounding = 6,
        blur   = { enabled = true, size = 6, passes = 2 },
        -- Pink glow instead of a dark drop shadow.
        shadow = { enabled = true, range = 18, render_power = 2, color = 0x66ff2bd6 },
    },
    animations = { enabled = true },
    input = {
        kb_layout    = "tr",
        follow_mouse = 1,
        touchpad     = { natural_scroll = true },
    },
    dwindle = { preserve_split = true },
    misc = {
        disable_hyprland_logo    = true,
        disable_splash_rendering = true,
        force_default_wallpaper  = 0,
        background_color         = 0xff0b0221,
    },
})

hl.curve("smooth", { type = "bezier", points = { {0.25, 1}, {0.5, 1} } })
hl.animation({ leaf = "windows",    enabled = true, speed = 5, bezier = "smooth", style = "popin 80%" })
hl.animation({ leaf = "fade",       enabled = true, speed = 5, bezier = "smooth" })
hl.animation({ leaf = "workspaces", enabled = true, speed = 5, bezier = "smooth", style = "slide" })

hl.gesture({ fingers = 3, direction = "horizontal", action = "workspace" })

-- Apps
hl.bind(mainMod .. " + Return", hl.dsp.exec_cmd(terminal))
hl.bind(mainMod .. " + Space",  hl.dsp.exec_cmd(menu))
hl.bind(mainMod .. " + E",      hl.dsp.exec_cmd(files))
hl.bind(mainMod .. " + L",      hl.dsp.exec_cmd("hyprlock"))
hl.bind(mainMod .. " + N",      hl.dsp.exec_cmd("swaync-client -t -sw"))
hl.bind(mainMod .. " + M",      hl.dsp.exec_cmd("osmium-mode menu"))
hl.bind(mainMod .. " + I",      hl.dsp.exec_cmd("osmium-settings"))
hl.bind("Print", hl.dsp.exec_cmd('grim -g "$(slurp)" - | wl-copy'))

-- Windows
hl.bind(mainMod .. " + Q",         hl.dsp.window.close())
hl.bind(mainMod .. " + F",         hl.dsp.window.fullscreen({ action = "toggle" }))
hl.bind(mainMod .. " + V",         hl.dsp.window.float({ action = "toggle" }))
hl.bind(mainMod .. " + SHIFT + E", hl.dsp.exit())
hl.bind(mainMod .. " + left",  hl.dsp.focus({ direction = "left" }))
hl.bind(mainMod .. " + right", hl.dsp.focus({ direction = "right" }))
hl.bind(mainMod .. " + up",    hl.dsp.focus({ direction = "up" }))
hl.bind(mainMod .. " + down",  hl.dsp.focus({ direction = "down" }))
hl.bind(mainMod .. " + mouse:272", hl.dsp.window.drag(),   { mouse = true })
hl.bind(mainMod .. " + mouse:273", hl.dsp.window.resize(), { mouse = true })

-- Workspaces 1-9
for i = 1, 9 do
    hl.bind(mainMod .. " + " .. i,         hl.dsp.focus({ workspace = i }))
    hl.bind(mainMod .. " + SHIFT + " .. i, hl.dsp.window.move({ workspace = i }))
end

-- Media keys
local media = { locked = true, repeating = true }
hl.bind("XF86AudioRaiseVolume",  hl.dsp.exec_cmd("swayosd-client --output-volume raise"), media)
hl.bind("XF86AudioLowerVolume",  hl.dsp.exec_cmd("swayosd-client --output-volume lower"), media)
hl.bind("XF86AudioMute",         hl.dsp.exec_cmd("swayosd-client --output-volume mute-toggle"), media)
hl.bind("XF86MonBrightnessUp",   hl.dsp.exec_cmd("swayosd-client --brightness raise"), media)
hl.bind("XF86MonBrightnessDown", hl.dsp.exec_cmd("swayosd-client --brightness lower"), media)

-- Appearance chosen in the settings app (Super+I). Optional: missing file is fine.
local settings = (os.getenv("XDG_CONFIG_HOME") or (os.getenv("HOME") .. "/.config")) .. "/hypr/osmium-settings.lua"
local chunk = loadfile(settings)
if chunk then chunk() end

-- First-login welcome window (osmium-welcome) floats in the middle.
hl.window_rule({
    name  = "osmium-welcome",
    match = { class = "^osmium-welcome$" },
    float  = true,
    center = true,
})
