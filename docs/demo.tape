# The README's demo. Record it again from the repository's root with:
#
#   vhs docs/demo.tape
#
# It needs vhs (brew install vhs) and an installed svm. It runs svm in folders
# of its own, so the machines on this computer are left alone and don't show.

Output docs/demo.gif

Require svm

Set Shell "bash"
Set FontFamily "JetBrains Mono, SF Mono, Menlo, monospace"
Set FontSize 20
Set Width 1200
Set Height 700
Set Padding 24
Set Margin 32
Set MarginFill "#6c5ce7"
Set BorderRadius 12
Set WindowBar Colorful
Set Theme "Catppuccin Mocha"
Set TypingSpeed 45ms
Set Framerate 30
Set PlaybackSpeed 1.0
Set CursorBlink false

Hide
Type `export SVM_HOME="$(mktemp -d)" SVM_RUNTIME="$(mktemp -d /tmp/svm.XXXX)"` Enter
Type `export PS1='\[\e[38;5;141m\]❯\[\e[0m\] '` Enter
Type "clear" Enter
Show

Sleep 600ms
Type "svm images" Sleep 300ms Enter
Sleep 1.8s

Type "svm create debian-13 --name dev --user agent" Sleep 300ms Enter
Wait+Screen /Created dev/
Sleep 800ms

Type "svm start dev" Sleep 300ms Enter
Wait+Screen /Started dev/
Sleep 500ms

Type "svm wait dev" Sleep 300ms Enter
Wait+Screen@30s /dev is ready/
Sleep 800ms

Type "svm list" Sleep 300ms Enter
Sleep 1.8s

Type "svm exec dev -- uname -srm" Sleep 300ms Enter
Sleep 1.5s

Type `echo 'import sys; print(sys.version)' | svm exec dev -- python3` Sleep 300ms Enter
Sleep 2s

Type "svm delete dev --force" Sleep 300ms Enter
Wait+Screen /Deleted dev/
Sleep 2.5s

Hide
Type `rm -rf "$SVM_HOME" "$SVM_RUNTIME"` Enter
