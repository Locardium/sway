# Product

## Register

product

## Users

DJs (starting with the author himself) who organize their local music library: import folders of FLAC/MP3, build folder/playlist hierarchies for sets, pre-listen to tracks, and then export to legacy DJ software (Rekordbox via iTunes XML, Serato). Usage context: long organization sessions on desktop, typically at night, single screen, lots of keyboard and drag&drop.

## Product Purpose

Sway is a cross-platform DJ player/organizer (Tauri). It replaces the iTunes/Explorer flow for curating the library: importing, tagging, manually ordering hierarchical playlists, and projecting that hierarchy to legacy formats. Success = organizing a 50-track set faster and more comfortably than in Rekordbox itself, without losing metadata.

## Brand Personality

Deep, precise, fluid. Dark booth interface — true deep black (not dark gray), a single cold blue/light-blue accent evoking "sway" (movement, water). High information density with clear hierarchy; the track and its metadata are the protagonists.

## Anti-references

- Generic Spotify/streaming consumer clone (giant album cards, hero banners, "recommended for you").
- Cold Excel/foobar2000-style spreadsheet with no visual hierarchy.
- Orange/coral as accent (explicitly discarded despite the Playcloud reference; taken from the reference: deep black, floating player bar, collapsible right panel, minimal top bar).

## Design Principles

1. **The table is the instrument** — the track view is where you live; high density, sacred manual order, configurable columns.
2. **Drag&drop is the main verb** — everything that gets organized is dragged; drop targets and indicators must be obvious.
3. **Booth darkness** — deep black, high contrast in primary text, the accent earns its place (playing, drop targets, focus).
4. **No ceremony** — direct actions: right click, double click, Delete, shortcuts; no wizards or unnecessary confirmations (only for destructive actions).
5. **Legacy-first** — nothing in the UI can break the projection to iTunes XML/Serato (folder/playlist hierarchy, manual order).

## Accessibility & Inclusion

Minimum WCAG AA contrast (4.5:1 normal text) on dark backgrounds. Reduced motion respected throughout all animation. Click targets ≥ 24px. Keyboard operation in the table (selection, Delete, Ctrl+A) and modals (Enter/Escape).
