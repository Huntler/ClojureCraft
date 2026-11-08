Build me a small Clojure library for analyzing my workout log. I export it as text, one set per line, like:

```
2026-03-02, Bench Press, 3x8 @ 60kg
2026-03-02, squat, 5x5 @ 100 kg
2026-03-04, Pull-up, 3x10 @ 0kg
```

Parse these lines into maps (skip blank or malformed lines, and normalize exercise names so "bench press" and "Bench Press" count as the same). From that data I want the training volume (sets × reps × weight) per session and per ISO week, my personal record for each exercise (heaviest weight, and an estimated one-rep max using the Epley formula), my longest streak of weeks with at least two sessions, and a list of exercises where my best weight dropped compared to the previous week. Finally, give me a weekly summary as a plain-text report I can paste into my notes.

Keep everything as pure functions over plain data (no I/O, no external libraries beyond clojure.core, clojure.string and clojure.set), and include a few example inputs and outputs for each function so I can see it works.
