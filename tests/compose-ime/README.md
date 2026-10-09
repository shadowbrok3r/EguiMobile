# compose-ime

A keyboard without keys, for the emulator. The emulator's Gboard commits every letter as it is
typed, so it never leaves a word composing, and IME bugs that only show with a composing keyboard
(Samsung Keyboard, or Gboard on a phone) do not reproduce with it. compose-ime makes the
InputConnection calls a typing keyboard makes, on adb broadcasts, so a check can hold a word in
composition and then tap, rotate or press Back.

Build, install and select it:

```bash
tests/compose-ime/build.sh
adb install -r tests/compose-ime/build/compose-ime.apk
adb shell ime enable com.github.egui_mobile.composeime/.ComposeIme
adb shell ime set com.github.egui_mobile.composeime/.ComposeIme
```

Drive it. Each broadcast logs a `ComposeIme` line saying whether a field was connected:

```bash
adb shell am broadcast -a egui.ime.COMPOSE --es text hello     # setComposingText
adb shell am broadcast -a egui.ime.COMMIT --es text "'hello '" # commitText
adb shell am broadcast -a egui.ime.FINISH                      # finishComposingText
adb shell am broadcast -a egui.ime.ENTER                       # Enter down and up
```

`adb shell ime reset` goes back to the default keyboard.

## A composing word survives leaving the field

In android-hello, tap **Note**, COMPOSE `hello`, then hold a tap on empty space outside the field
for a quarter second (`adb shell input swipe X Y X Y 250`). The field must still read `hello`.
Also: a tap inside the word keeps it and moves the caret there; Back keeps it; COMMIT `hello `
then COMPOSE `world` and a tap outside leaves `hello world`.
