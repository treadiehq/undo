# Undo launch video

A 42-second, 1920×1080 Remotion launch video for Undo. The story is built
around the highest-value distinction: reverse the bad part of an agent run
without throwing away the work worth keeping.

## Conversion strategy

1. Open with one memorable promise: undo the bad, keep the good.
2. Show the real workflow in restrained, editorial product scenes.
3. Make the selective result concrete: dashboard kept, auth restored.
4. Establish trust with preview-first and local-only behavior.
5. End on the official Undo mark and one installation command.

## Preview

```bash
cd video
npm install
npm run dev
```

The original soundtrack is generated locally before Studio opens.

## Render

```bash
npm run render
npm run render:poster
```

Outputs are written to `video/out/`. The video source is deterministic and does
not depend on remote fonts, stock footage, or third-party media.

The logo in `public/undo-logo.png` is the official mark from `useundo.co`.
