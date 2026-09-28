-- The file list, drawn on mpv's own OSD.
--
-- mpv will not open a chooser, but it will hold an idle window, draw ASS into it and
-- hand us the keys, which is the whole of a list: a cursor that moves without
-- playing anything, and a key that commits. Nothing starts until you pick it.
--
-- One process throughout. The alternative was a separate program that lists files
-- and then hands over to a player, and every handover is a window closing and
-- another opening, which on this panel has already cost us an afternoon of blank
-- frames.
--
-- The ASS canvas is set to the panel's own 256x144, so a size in a style tag is a
-- size in pixels and the layout can be reasoned about directly.

local mp = require 'mp'

local W, H = 256, 144
local ROW = 15
local TOP = 16
local ROWS = math.floor((H - TOP - 4) / ROW)

local files, labels = {}, {}
local sel, first = 1, 1
local listing = true

local function basename(path)
    return path:match("([^/]+)$") or path
end

-- ASS takes { and } as its own, and a filename is somebody else's string.
local function escape(s)
    return s:gsub("\\", "\\\\"):gsub("{", "\\{"):gsub("}", "\\}")
end

local function load_list(path)
    local fd = io.open(path, "r")
    if not fd then
        return
    end
    for line in fd:lines() do
        if line ~= "" then
            files[#files + 1] = line
            labels[#labels + 1] = basename(line)
        end
    end
    fd:close()
end

local function draw()
    if not listing then
        mp.set_osd_ass(W, H, "")
        return
    end
    if #files == 0 then
        mp.set_osd_ass(W, H, string.format(
            "{\\an7\\pos(0,0)\\bord0\\shad0\\1c&HFFFFFF&\\p1}m 0 0 l %d 0 l %d %d l 0 %d{\\p0}\n"
            .. "{\\an7\\pos(4,4)\\fs13\\bord0\\shad0\\1c&H000000&}"
            .. "No videos found.\\NPut some in your home folder.", W, W, H, H))
        return
    end

    -- The window follows the cursor rather than paging, so a held key scrolls
    -- smoothly instead of jumping a screen at a time.
    if sel < first then
        first = sel
    elseif sel >= first + ROWS then
        first = sel - ROWS + 1
    end

    -- The list paints its own ground. mpv's idle window is black, and this panel's
    -- own screens are dark on light, so a list drawn straight onto it would be black
    -- on black. Playback is left alone: a video fills the window itself.
    local out = {
        string.format(
            "{\\an7\\pos(0,0)\\bord0\\shad0\\1c&HFFFFFF&\\p1}m 0 0 l %d 0 l %d %d l 0 %d{\\p0}",
            W, W, H, H),
        string.format("{\\an7\\pos(4,2)\\fs11\\bord0\\shad0\\1c&H000000&}%d / %d", sel, #files),
    }
    for i = first, math.min(first + ROWS - 1, #files) do
        local y = TOP + (i - first) * ROW
        local name = escape(labels[i])
        if i == sel then
            -- Both a bar and an arrow. The bar reads better at this size, but it is
            -- one ASS drawing command away from being invisible, and a list whose
            -- cursor cannot be seen is a list that cannot be used, so the arrow is
            -- there as the thing that cannot fail to draw.
            out[#out + 1] = string.format(
                "{\\an7\\pos(0,%d)\\bord0\\shad0\\1c&H000000&\\p1}m 0 0 l %d 0 l %d %d l 0 %d{\\p0}",
                y - 1, W, W, ROW, ROW)
            out[#out + 1] = string.format(
                "{\\an7\\pos(2,%d)\\fs13\\bord0\\shad0\\1c&HFFFFFF&}> %s", y, name)
        else
            out[#out + 1] = string.format(
                "{\\an7\\pos(2,%d)\\fs13\\bord0\\shad0\\1c&H000000&}  %s", y, name)
        end
    end
    mp.set_osd_ass(W, H, table.concat(out, "\n"))
end

local function show_list()
    listing = true
    mp.set_property_bool("pause", true)
    draw()
end

local function hide_list()
    listing = false
    draw()
end

local function play()
    if #files == 0 then
        return
    end
    hide_list()
    -- loadfile is a request, not a load: the file arrives later, and unpausing here
    -- races the arrival. file-loaded below is the moment there is something to
    -- unpause.
    mp.commandv("loadfile", files[sel], "replace")
end

local function move(by)
    if #files == 0 then
        return
    end
    sel = math.max(1, math.min(#files, sel + by))
    draw()
end

-- One set of bindings, branching on the mode, because a key that means two things is
-- easier to hold in the head than a key that exists half the time.
mp.add_forced_key_binding("UP", "up", function()
    if listing then move(-1) else mp.commandv("add", "volume", 5) end
end, { repeatable = true })

mp.add_forced_key_binding("DOWN", "down", function()
    if listing then move(1) else mp.commandv("add", "volume", -5) end
end, { repeatable = true })

mp.add_forced_key_binding("LEFT", "left", function()
    if listing then move(-ROWS) else mp.commandv("seek", -5) end
end, { repeatable = true })

mp.add_forced_key_binding("RIGHT", "right", function()
    if listing then move(ROWS) else mp.commandv("seek", 5) end
end, { repeatable = true })

mp.add_forced_key_binding("ENTER", "enter", function()
    if listing then play() else mp.commandv("cycle", "pause") end
end)

-- The one key that crosses between the two modes, in both directions.
mp.add_forced_key_binding("x", "list", function()
    if listing then
        if mp.get_property("path") then hide_list() end
    else
        show_list()
    end
end)

mp.add_forced_key_binding("b", "info", function()
    if listing then
        return
    end
    mp.osd_message(string.format("%s\n%s / %s   vol %s",
        mp.get_property("filename") or "",
        mp.get_property_osd("time-pos") or "0",
        mp.get_property_osd("duration") or "0",
        mp.get_property_osd("volume") or "0"), 3)
end)

mp.add_forced_key_binding("z", "leave", function()
    if listing then mp.commandv("quit") else show_list() end
end)

-- Playback proper starts here rather than beside the loadfile that asked for it.
mp.register_event("file-loaded", function()
    if not listing then
        mp.set_property_bool("pause", false)
    end
end)

-- A file that ends or fails puts the list back rather than leaving a dead window.
--
-- Only those two reasons. Replacing one file with another also ends a file, with
-- reason "stop", and treating that as the end of playback made every play after the
-- first load the file and then immediately pause it and return to the list.
mp.register_event("end-file", function(event)
    if event.reason == "eof" or event.reason == "error" then
        show_list()
    end
end)

load_list(mp.get_opt("browser-list") or "")
show_list()
