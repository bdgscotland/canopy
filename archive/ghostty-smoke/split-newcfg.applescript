-- OQ1 variant B: surface configuration via the `new surface configuration`
-- command plus property assignment. The alternative to the record literal.
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
			set cfg to new surface configuration
			set initial working directory of cfg to theCwd
			set command of cfg to "shell:" & quoted form of exePath
			set wait after command of cfg to false
			set environment variables of cfg to envList
		on error errMsg
			return "ERR:newcfg-rejected:" & errMsg
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
