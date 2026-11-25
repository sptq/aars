-- Etap 13: pełna treść śledzonej pozycji (ta, która idzie na antenę).
ALTER TABLE source_items ADD COLUMN text TEXT NOT NULL DEFAULT '';
UPDATE source_items SET text = summary WHERE text = '';
