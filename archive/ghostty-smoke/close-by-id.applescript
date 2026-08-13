-- OQ6: can a terminal be addressed by its id for `close`, or must we iterate?
-- Tries direct id specifier first, then falls back to iterating terminals.
--
-- argv: 1=terminalID
-- returns: "OK:direct" | "OK:iterate" | "ERR:not-found" | "ERR:<reason>"

on run argv
	set wantID to item 1 of argv

	tell application "Ghostty"
		-- Attempt 1: direct id specifier
		try
			set t to first terminal of front window whose id is wantID
			close t
			return "OK:direct"
		on error errMsg
			-- fall through to iteration
		end try

		-- Attempt 2: iterate every terminal in every window
		try
			repeat with w in windows
				repeat with t in terminals of w
					if id of t is wantID then
						close t
						return "OK:iterate"
					end if
				end repeat
			end repeat
		on error errMsg
			return "ERR:" & errMsg
		end try

		return "ERR:not-found"
	end tell
end run
