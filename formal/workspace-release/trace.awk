# Prints a TLC counterexample one state per line: the action that led to it
# and only the variables it changed. The first line has every variable.

# A liveness counterexample ends in a loop back to an earlier state, or in
# stuttering forever. First, since TLC prints "State N: Stuttering".
/^Back to state [0-9]+:/ || /Stuttering/ {
    flush()
    line = $0
    sub(/ line [0-9].*/, "", line)
    sub(/: </, " by ", line)
    print "loop: " line ", and so on forever"
    next
}
/^State [0-9]+:/ {
    flush()
    step = $2
    sub(/:$/, "", step)
    action = $3
    sub(/^</, "", action)
    changed = ""
    next
}
/^\/\\ / {
    sub(/^\/\\ /, "")
    name = $1
    value = substr($0, length(name) + 4)
    if (seen[name] != value) changed = changed " " name "=" value
    seen[name] = value
    next
}
function flush() {
    if (step != "") print step " " action ":" changed
    step = ""
}
END { flush() }
