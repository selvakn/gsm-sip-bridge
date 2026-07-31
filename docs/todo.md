
Observed pending items
----------------------

- [x] ~~Auto-discovered VoLTE modems weren't in the circuit-switched
      daemon's exclusion set~~ — flagged by Greptile on PR #9, confirmed
      pre-existing against commit eb26303. Fixed independently by
      455594b (`specs/020-volte-line-netns`): `modules::discovery` now
      reads the VoLTE line manifest (`active_volte_line_ports`/
      `active_volte_card_ids`) the same way it already read VoWiFi's line
      file, so a fully auto-discovered VoLTE line is excluded too, not
      just explicitly-pinned `[[volte.line]]` ones.

