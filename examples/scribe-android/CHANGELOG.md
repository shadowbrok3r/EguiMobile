# Changelog

## v0.2.2 — 2026-11-11

A note's screen is rearranged. The player moved to a bar at the bottom that is always in view, with the buttons beside it gathered into two menus: Share (Copy, Share text, Share file, Share audio) and More (Pin, Transcribe again, Delete). The text scrolls in its own area, at most about 70% of the screen, and Speakers, Transcript and Timeline fold away under their headings. Each speaker has a colour: it bands their turns in the text and marks their rows in the timeline and their name under Speakers. The timeline lights the part being played, and tapping the text puts the cursor on the word you tapped instead of at the end.

## v0.2.1 — 2026-11-11

New: Share file on a note sends its text as a .txt attachment through the share sheet, instead of pasting a long transcript into a message. It is the note's own file from Documents/Scribe, brought up to date first, so nothing is duplicated.

## v0.2.0 — 2026-11-11

Each finished note's text is now also saved as a text file in Documents/Scribe on the phone, named by date and title, so a transcript can be attached anywhere without copying it. The file follows the note's edits and stays if the note is deleted; notes you already have get theirs on first launch. Switch it off under Settings > Transcription. New: speakers are told apart. After a recording is transcribed its voices are compared on the NPU and each speaker's turns are labelled Speaker 1, Speaker 2; open Speakers on a note to name them or to set how many there are. It leans towards fewer speakers and gives each transcript segment to one person, so quick exchanges can be merged. Settings > Debug > Self-test now also checks the speaker model.

## v0.1.0 — 2026-11-10

First release. Voice notes transcribed on the phone's NPU with Whisper: record with the screen off or the app swiped away, and the text appears as you speak. Every recording is kept as a WAV, so any note can be transcribed again later with another model or language. Models download under Settings (173 MB to 1.88 GB, use Wi-Fi). Not yet run on a phone: if no text appears, Settings > Debug > Self-test says why.
