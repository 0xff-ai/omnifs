--- omnifs-inspect.yazi — live native Yazi previewer for the omnifs inspector.
---
--- Renders the omnifs inspector stream as correlated, per-operation traces
--- grouped by FUSE `trace_id`, drawn with Yazi's own widgets and refreshed in
--- place. It is a pure consumer of the existing inspect protocol: each refresh
--- shells out to `omnifs inspect --dump` for a one-shot snapshot of the
--- daemon's history ring (the same records served on the :7878 socket).
---
--- Traces only — there is deliberately no cache viewer.
---
--- The previewer ignores the hovered file's contents; the file is only a
--- trigger. `omnifs features add yazi` installs this plugin, registers the
--- previewer, and drops a sentinel `*.omnifs-inspect` file to hover.
---
--- Requires the `omnifs` CLI on PATH (the inspector socket is published to the
--- host by `omnifs up` / `omnifs dev`, so this works host-side too).

local M = {}

local DIM = "#808080"
local MAX_TRACES = 60
-- Seconds between in-place refreshes while the file stays hovered.
local REFRESH = 1.5

local function fstr(line, key)
	return line:match('"' .. key .. '":"([^"]*)"') or ""
end

local function fnum(line, key)
	return line:match('"' .. key .. '":(%-?%d+)')
end

-- Extract the fields we render from one raw inspector JSONL record. Keys are
-- unique across the envelope and the flattened event, so anchored matches are
-- unambiguous. Returns nil for lines without an event type.
local function parse(line)
	local typ = fstr(line, "type")
	if typ == "" then
		return nil
	end
	local op, method, kind = fstr(line, "op"), fstr(line, "method"), fstr(line, "kind")
	local path, summary = fstr(line, "path"), fstr(line, "summary")
	local remote, tree_ref = fstr(line, "remote"), fstr(line, "tree_ref")
	return {
		trace_id = fnum(line, "trace_id") or "?",
		typ = typ,
		a = (op ~= "" and op) or (method ~= "" and method) or kind,
		b = (path ~= "" and path) or (summary ~= "" and summary) or (remote ~= "" and remote) or tree_ref,
		elapsed = fnum(line, "elapsed_us"),
		outcome = fstr(line, "outcome"),
		provider = fstr(line, "provider"),
	}
end

local function fmt_elapsed(us)
	local n = tonumber(us)
	if not n then
		return ""
	elseif n >= 1000 then
		return string.format("%.1fms", n / 1000)
	end
	return string.format("%dµs", n)
end

local function indent_for(typ)
	if typ == "callout.start" or typ == "callout.end" or typ == "cache.event" then
		return 4
	elseif typ == "fuse.start" or typ == "fuse.end" then
		return 0
	end
	return 2
end

local function outcome_span(oc)
	return ui.Span("  " .. oc):fg(oc == "ok" and "green" or "red")
end

-- Bold header for one trace: the FUSE op + path, with its outcome/elapsed.
local function header_line(trace_id, fstart, fend)
	local op = (fstart and fstart.a ~= "" and fstart.a) or "?"
	local spans = {
		ui.Span("● "):fg("blue"),
		ui.Span("trace " .. trace_id .. "  "):fg(DIM),
		ui.Span(op):bold(),
	}
	if fstart and fstart.b ~= "" then
		spans[#spans + 1] = ui.Span("  " .. fstart.b)
	end
	if fend then
		local el = fmt_elapsed(fend.elapsed)
		if el ~= "" then
			spans[#spans + 1] = ui.Span("  " .. el):fg(DIM)
		end
		if fend.outcome ~= "" then
			spans[#spans + 1] = outcome_span(fend.outcome)
		end
	end
	return ui.Line(spans)
end

-- Indented detail line for a provider/callout/cache/clone/subtree record.
local function detail_line(r)
	local label
	if r.typ:find("^provider") then
		label = (r.provider ~= "" and r.provider or "provider") .. (r.a ~= "" and ("." .. r.a) or "")
	elseif r.typ:find("^callout") then
		label = "callout " .. r.a
	elseif r.typ:find("^cache") then
		label = "cache " .. r.a
	elseif r.typ:find("^clone") then
		label = "clone"
	elseif r.typ:find("^subtree") then
		label = "subtree"
	else
		label = r.typ
	end

	local spans = { ui.Span(string.rep(" ", indent_for(r.typ))), ui.Span(label):fg("cyan") }
	if r.b ~= "" then
		spans[#spans + 1] = ui.Span("  " .. r.b)
	end
	local el = fmt_elapsed(r.elapsed)
	if el ~= "" then
		spans[#spans + 1] = ui.Span("  " .. el):fg(DIM)
	end
	if r.outcome ~= "" then
		spans[#spans + 1] = outcome_span(r.outcome)
	end
	return ui.Line(spans)
end

local function msg(job, text, color)
	ya.preview_widget(job, ui.Text({ ui.Line({ ui.Span(text):fg(color or DIM) }) }):area(job.area))
end

-- Group parsed rows into trace blocks and render newest-first.
local function render(job, jsonl)
	local order, groups = {}, {}
	for line in jsonl:gmatch("[^\n]+") do
		local r = parse(line)
		if r then
			if not groups[r.trace_id] then
				groups[r.trace_id] = {}
				order[#order + 1] = r.trace_id
			end
			local g = groups[r.trace_id]
			g[#g + 1] = r
		end
	end

	local lines, shown = {}, 0
	for i = #order, 1, -1 do
		if shown >= MAX_TRACES then
			break
		end
		local rows = groups[order[i]]
		local fstart, fend
		for _, r in ipairs(rows) do
			if r.typ == "fuse.start" then
				fstart = r
			elseif r.typ == "fuse.end" then
				fend = r
			end
		end
		lines[#lines + 1] = header_line(order[i], fstart, fend)
		for _, r in ipairs(rows) do
			if r.typ ~= "fuse.start" and r.typ ~= "fuse.end" then
				lines[#lines + 1] = detail_line(r)
			end
		end
		lines[#lines + 1] = ui.Line("")
		shown = shown + 1
	end

	if #lines == 0 then
		return msg(job, "omnifs-inspect: no inspector records yet (is the daemon busy?)")
	end

	local view, skip = {}, job.skip or 0
	for i = skip + 1, #lines do
		view[#view + 1] = lines[i]
	end
	ya.preview_widget(job, ui.Text(view):area(job.area))
end

function M:peek(job)
	local out, err = Command("omnifs")
		:arg({ "inspect", "--dump" })
		:stdout(Command.PIPED)
		:stderr(Command.PIPED)
		:output()

	if not out then
		msg(job, "omnifs-inspect: cannot run `omnifs inspect --dump`: " .. tostring(err), "red")
	elseif (out.stdout or "") == "" then
		local hint = out.stderr ~= "" and out.stderr:gsub("%s+$", "")
			or "no inspector reachable (try `omnifs up` or `omnifs dev`)"
		msg(job, "omnifs-inspect: " .. hint, "red")
	else
		render(job, out.stdout)
	end

	-- Refresh in place: re-trigger this preview after a beat. `only_if`
	-- makes it a no-op once the cursor moves off the file, and the sleep
	-- is the cancellation point when navigating away.
	ya.sleep(REFRESH)
	ya.emit("peek", { job.skip or 0, only_if = job.file.url })
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
