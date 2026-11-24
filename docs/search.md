# Offline search (format version 1)

A search index is a small file beside an archive that finds its names with
no network: towns and villages, points of interest, streets, peaks, lakes
and rivers, and the `sar` layer's huts, springs, emergency points and
trails. It is built from the archive itself, so it holds what the map shows,
and it can be built again on a phone from an archive cut with `extract`.

```sh
# With a compile
osm-tile-compiler compile malopolskie.osm.pbf --output malopolskie.pmtiles --sources sources/ --sar --search-index
# Or from an archive that already exists
osm-tile-compiler search-index malopolskie.pmtiles          # writes malopolskie-search.idx
osm-tile-compiler search malopolskie.pmtiles hala gasienicowa
osm-tile-compiler search malopolskie.pmtiles dluga krakow --near 19.94 50.06 --limit 5 --json
```

From Rust, set `BasemapRequest::search_index`, or call `write_search_index`;
query with `SearchIndex::open` and `SearchIndex::search(&SearchQuery)`.

## Matching

- Text is folded before it is compared (`fold_search_text`): lowercase, no
  diacritics, and `ł`, `đ`, `ø`, `ß`, `æ`, `œ`, `þ` mapped by hand. Anything
  that is not a letter or a digit separates words, so `Łódź`, `lodz` and
  `ŁÓDŹ` are the same, as are `Bielsko-Biała` and `bielsko biala`.
- A query is a set of words. Every word must match a word of the record's
  names (`name`, `name_en`, `name_de`, `name_int`, `name:latin`, and a `sar`
  point's `ref`), or a word of its context, the nearest city, town or
  village: `florianska krakow` finds Floriańska in Kraków.
- The last word matches as a prefix unless the query ends in a space, so a
  search box can query on every key press. Another word that matches no
  word exactly also matches as a prefix.
- `ul`, `ulica`, `al`, `aleja`, `aleje`, `pl`, `plac`, `os` and `osiedle`
  are dropped from a query with other words, since Polish street names in
  OSM mostly leave them out.
- Hits are ranked by how prominent the record is (a city before a village,
  a village before a shop; capitals, high peaks, long streets and anything
  in a city a little higher), then by whether the whole name matches, and,
  with `near`, by distance: each doubling of the distance costs about as
  much as half a tier.
- `within` keeps hits inside a box, and `layers` hits from some layers.

There is no typo tolerance, no house numbers (the `housenumber` layer has no
street names), and no admin boundaries: the context is the nearest
settlement, within 15 km of a city's point, 6 km of a town's and 3 km of a
village's, the one nearest for its size.

## What the index holds

The highest zoom of the archive (z14 for a full compile) is read: the
`place`, `aerodrome_label`, `mountain_peak`, `sar`, `poi`, `park`,
`water_name`, `waterway` and `transportation_name` layers.

- Points become one record per OSM object, by the tiles' feature ids, so a
  label that is in the buffers of the tiles around it is read once. An object
  in two layers (a hut in `poi` and `sar`) is kept from the first layer in
  that order. A point without an id is kept from the tile it lies in.
- Named lines are cut into pieces at tile edges. The pieces of one name
  whose ends lie within 300 m of each other are joined into one record,
  placed at the piece middle nearest the middle of them all. A street that
  changes class keeps its most important class. Two streets of one name in
  one town that meet are one record.
- Polygons are not read: the layers that have named polygons (`park`) also
  have points for them.

| Region | Archive | Index | Records | Build (4 threads) |
| --- | --- | --- | --- | --- |
| Berlin | 41.9 MB | 4.2 MB (10%) | 86,550 | 1.0 s |
| Małopolskie | 153.7 MB | 4.9 MB (3.2%) | 109,982 | 2.5 s |
| Pomorskie | 101.4 MB | 3.5 MB (3.5%) | 79,545 | 1.4 s |

Berlin's share is high because it has about 70,000 named POIs. On the
cloud box (4 cores), opening the Małopolskie index takes 0.3 ms, and a query
on every key press of `kraków`, `floriańska krakow` or `biedronka` takes 0.3
to 2 ms on average and 6 ms at worst (a one-letter query near Kraków).
Phones are not measured yet.

## Reading it

Opening an index reads its header, its kinds and its dictionary index: for
Małopolskie about 25 KB. A query reads the dictionary blocks of its words,
their record lists, the fixed rows of the records it weighs (7 bytes each,
read in runs) and the names of the few it returns. Nothing else stays in
memory, and one `SearchIndex` serves queries from several threads.

## File layout

All numbers are little-endian. A varint is LEB128, as in protobuf.

| Bytes | Field |
| --- | --- |
| 8 | Magic `OTCSRCH\0` |
| 4 | `u32` format version, 1 |
| 4 | `u32` section count, 9 |
| 9 × 16 | Each section's `u64` offset from the start of the file and `u64` length |

The sections, in order:

0. **Metadata**: JSON, `format` (`search-index`), `version`, `records`,
   `source` (the archive's file name), `zoom` (the zoom read), and the
   archive's `bounds` and `osm_data_timestamp`.
1. **Kinds**: JSON, an array of `[layer, class, subclass, tier]`, tier 0 (a
   country) to 9.
2. **Points**: per record, longitude and latitude as `i32` in 1e-7 degrees,
   8 bytes.
3. **Attributes**: per record, the kind (`u16`, an index into kinds), the
   context record (`u32`, `0xFFFFFFFF` for none) and the rank (`u8`, the tier
   × 10 less the record's boosts, lower is more prominent), 7 bytes.
4. **Record blocks**: a `u32` offset into the records section for every 64
   records, then the section's length.
5. **Records**: per record, a varint count of names, each a varint length and
   UTF-8 bytes, the first being `name`; a flags byte (1 `ele`, 2 `ref`, 4
   feature id); then, as flagged, `ele` as a zigzag varint, `ref` as a varint
   length and bytes, and the feature id as a varint.
6. **Dictionary index**: per block of 64 words, the block's `u32` offset in
   the dictionary, its first word's `u32` offset in the postings, and its
   first word as a varint length and bytes.
7. **Dictionary**: the folded words in byte order, each as a varint count of
   bytes shared with the previous word in its block, a varint length and the
   remaining bytes, a varint count of records, the first record id as a
   varint, and the varint length of the rest of its record list.
8. **Postings**: for each word in order, the rest of its record ids as
   varint gaps from the previous one.

Records are ordered by rank, then by folded name, so a word's first records
are its most prominent ones.

The version changes when a reader of the old version would misread a file:
the layout, the folding, or what a field means (see `docs/formats.md`).
