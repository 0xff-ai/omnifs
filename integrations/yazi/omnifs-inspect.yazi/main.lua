--- omnifs-inspect.yazi — native Yazi previewer for the omnifs inspector stream.
---
--- Renders the omnifs inspector JSONL (the exact records the daemon serves on
--- its :7878 socket) as correlated per-operation traces, grouped by FUSE
--- `trace_id`, using Yazi's own widgets. No host/Rust changes: this is a pure
--- consumer of the existing inspect protocol.
---
--- This is a proof of concept: traces only, no cache viewer.
---
--- ── Try it (inside the omnifs container) ───────────────────────────────────
---   # 1. Tee the live inspector stream to a file (history snapshot + live):
---   bash -c 'exec 3<>/dev/tcp/127.0.0.1/7878; cat <&3 > /tmp/omnifs-inspect.jsonl &'
---   # 2. Generate some traffic so there is something to trace:
---   ls /github/torvalds; cat /dns/cloudflare.com/A
---   # 3. Open Yazi and hover the file:
---   yazi /tmp        # move the cursor onto omnifs-inspect.jsonl
---
--- ── Install ────────────────────────────────────────────────────────────────
---   cp -r omnifs-inspect.yazi ~/.config/yazi/plugins/
--- Register the previewer in ~/.config/yazi/yazi.toml:
---   [plugin]
---   prepend_previewers = [
---     { name = "*omnifs-inspect*.jsonl", run = "omnifs-inspect" },
---   ]
---
--- Requires `jq` on PATH (the same dependency as Yazi's built-in json previewer).

local M = {}

-- jq projection: one TSV row per inspector record. Columns:
--   1 seq  2 trace_id  3 type  4 a  5 b  6 elapsed_us  7 outcome
--   8 op_id  9 provider  10 mount
-- `a`/`b` pick the most useful context field per event variant; @tsv renders
-- missing fields (null) as empty strings.
local JQ = [[
[ .seq, .trace_id, .event.type,
  (.event.op // .event.method // .event.kind),
  (.event.path // .event.summary // .event.remote // .event.tree_ref),
  .event.elapsed_us, .event.outcome, .event.operation_id,
  .event.provider, .event.mount ] | @tsv
]]

local DIM = "#808080"
local MAX_TRACES = 60

-- Tab split that preserves trailing empty fields (Lua patterns drop them).
local function split_tab(s)
	local cols, start = {}, 1
	while true do
		local i = string.find(s, "\t", start, true)
		if not i then
			cols[#cols + 1] = string.sub(s, start)
			return cols
		end
		cols[#cols + 1] = string.sub(s, start, i - 1)
		start = i + 1
	end
end

local function fmt_elapsed(us)
	local n = tonumber(us)
	if not n then
		return ""
	end
	if n >= 1000 then
		return string.format("%.1fms", n / 1000)
	end
	return string.format("%dµs", n)
end

local function indent_for(t)
	if t == "callout.start" or t == "callout.end" or t == "cache.event" then
		return 4
	elseif t == "fuse.start" or t == "fuse.end" then
		return 0
	end
	return 2
end

local function outcome_span(oc)
	return ui.Span("  " .. oc):fg(oc == "ok" and "green" or "red")
end

-- Header line for one trace: the FUSE op + path, with its outcome/elapsed.
local function header_line(trace_id, fstart, fend)
	local op = (fstart and fstart[4] ~= "" and fstart[4]) or "?"
	local spans = {
		ui.Span("● "):fg("blue"),
		ui.Span("trace " .. trace_id .. "  "):fg(DIM),
		ui.Span(op):bold(),
	}
	if fstart and fstart[5] ~= "" then
		spans[#spans + 1] = ui.Span("  " .. fstart[5])
	end
	if fend then
		local el = fmt_elapsed(fend[6])
		if el ~= "" then
			spans[#spans + 1] = ui.Span("  " .. el):fg(DIM)
		end
		if fend[7] ~= "" then
			spans[#spans + 1] = outcome_span(fend[7])
		end
	end
	return ui.Line(spans)
end

-- Indented detail line for a provider/callout/cache/clone/subtree record.
local function detail_line(c)
	local t, a, b, provider = c[3], c[4], c[5], c[9]
	local label
	if t:find("^provider") then
		label = (provider ~= "" and provider or "provider") .. (a ~= "" and ("." .. a) or "")
	elseif t:find("^callout") then
		label = "callout " .. a
	elseif t:find("^cache") then
		label = "cache " .. a
	elseif t:find("^clone") then
		label = "clone"
	elseif t:find("^subtree") then
		label = "subtree"
	else
		label = t
	end

	local spans = { ui.Span(string.rep(" ", indent_for(t))), ui.Span(label):fg("cyan") }
	if b ~= "" then
		spans[#spans + 1] = ui.Span("  " .. b)
	end
	local el = fmt_elapsed(c[6])
	if el ~= "" then
		spans[#spans + 1] = ui.Span("  " .. el):fg(DIM)
	end
	if c[7] ~= "" then
		spans[#spans + 1] = outcome_span(c[7])
	end
	return ui.Line(spans)
end

local function msg(job, text, color)
	ya.preview_widget(job, ui.Text({ ui.Line({ ui.Span(text):fg(color or DIM) }) }):area(job.area))
end

function M:peek(job)
	local out, err = Command("jq")
		:arg({ "-r", JQ, tostring(job.file.path) })
		:stdout(Command.PIPED)
		:stderr(Command.PIPED)
		:output()

	if not out then
		return msg(job, "omnifs-inspect: jq failed to run: " .. tostring(err), "red")
	end

	-- Group rows by trace_id, preserving first-seen (seq) order within a trace.
	local order, groups = {}, {}
	for line in (out.stdout or ""):gmatch("[^\n]+") do
		local c = split_tab(line)
		if #c >= 10 then
			local id = c[2]
			if not groups[id] then
				groups[id] = {}
				order[#order + 1] = id
			end
			groups[id][#groups[id] + 1] = c
		end
	end

	-- Render newest traces first; cap so the preview stays snappy.
	local lines, shown = {}, 0
	for i = #order, 1, -1 do
		if shown >= MAX_TRACES then
			break
		end
		local rows = groups[order[i]]
		local fstart, fend
		for _, c in ipairs(rows) do
			if c[3] == "fuse.start" then
				fstart = c
			elseif c[3] == "fuse.end" then
				fend = c
			end
		end
		lines[#lines + 1] = header_line(order[i], fstart, fend)
		for _, c in ipairs(rows) do
			if c[3] ~= "fuse.start" and c[3] ~= "fuse.end" then
				lines[#lines + 1] = detail_line(c)
			end
		end
		lines[#lines + 1] = ui.Line("")
		shown = shown + 1
	end

	if #lines == 0 then
		local hint = (out.stderr ~= "" and out.stderr) or "no inspector records in this file"
		return msg(job, "omnifs-inspect: " .. hint)
	end

	-- Apply the scroll offset maintained by seek().
	local view, skip = {}, job.skip or 0
	for i = skip + 1, #lines do
		view[#view + 1] = lines[i]
	end
	ya.preview_widget(job, ui.Text(view):area(job.area))
end

function M:seek(job)
	local h = cx.active.current.hovered
	if not h or h.url ~= job.file.url then
		return
	end
	local step = math.floor(job.units * job.area.h / 10)
	step = step == 0 and ya.clamp(-1, job.units, 1) or step
	ya.emit("peek", { math.max(0, cx.active.preview.skip + step), only_if = job.file.url })
end

return M
