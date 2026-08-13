-- OQ1 variant A: surface configuration as an AppleScript RECORD LITERAL.
-- Also exercises OQ4 (shell: + quoted form survives a path with a space)
-- and returns the new terminal's id for OQ5/OQ6.
--
-- argv: 1=exePath  2=cwd  3..=KEY=VALUE env strings
-- returns: "OK|<treeID>|<focusVerified>"  or  "ERR:<reason>"

on run argv
	set exePath to item 1 of argv
	set theCwd to item 2 of argv
	set envList to items 3 thru -1 of argv

	tell application "Ghostty"
		if not frontmost then return "ERR:not-frontmost"
		set origTerm to focused terminal of selected tab of front window
		set origID to id of origTerm

		try
			set cfg to {initial working directory:theCwd, ¬
				command:"shell:" & quoted form of exePath, ¬
				wait after command:false, ¬
				environment variables:envList}
		on error errMsg
			return "ERR:record-literal-rejected:" & errMsg
		end try

		try
			set treeTerm to split origTerm direction right with configuration cfg
		on error errMsg
			return "ERR:split-failed:" & errMsg
		end try

		set treeID to id of treeTerm

		focus origTerm
		set verified to false
		repeat 20 times
			if id of focused terminal of selected tab of front window is origID then
				set verified to true
				exit repeat
			end if
			delay 0.01
		end repeat

		return "OK|" & treeID & "|" & verified
	end tell
end run
