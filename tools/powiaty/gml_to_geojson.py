"""Granice powiatów z PRG (GUGiK, WFS) -> GeoJSON do mapy w panelu.

Jednorazowe, ręczne (patrz README w tym katalogu). Tylko biblioteka standardowa.
Wejście: GML 3.1.1 z WFS, EPSG:4326 (kolejność osi: szerokość, długość).
Wyjście: FeatureCollection, properties {teryt, nazwa}, współrzędne [lon, lat].
"""
import json
import sys
import xml.etree.ElementTree as ET

GML = "{http://www.opengis.net/gml}"
MS = "{http://mapserver.gis.umn.edu/mapserver}"
WFS = "{http://www.opengis.net/wfs/2.0}"


TOLERANCE = 0.004  # stopnia (~300-450 m); uproszczenie Douglasa-Peuckera
PRECISION = 4


def _dist(p, a, b):
    (x, y), (x1, y1), (x2, y2) = p, a, b
    dx, dy = x2 - x1, y2 - y1
    if dx == 0 and dy == 0:
        return ((x - x1) ** 2 + (y - y1) ** 2) ** 0.5
    t = max(0.0, min(1.0, ((x - x1) * dx + (y - y1) * dy) / (dx * dx + dy * dy)))
    return ((x - x1 - t * dx) ** 2 + (y - y1 - t * dy) ** 2) ** 0.5


def simplify(pts, tol):
    keep = [False] * len(pts)
    keep[0] = keep[-1] = True
    stack = [(0, len(pts) - 1)]
    while stack:
        i, j = stack.pop()
        best, idx = 0.0, None
        for k in range(i + 1, j):
            d = _dist(pts[k], pts[i], pts[j])
            if d > best:
                best, idx = d, k
        if idx is not None and best > tol:
            keep[idx] = True
            stack += [(i, idx), (idx, j)]
    return [p for p, k in zip(pts, keep) if k]


def ring(pos_list):
    v = [float(x) for x in pos_list.text.split()]
    pts = [(v[i + 1], v[i]) for i in range(0, len(v), 2)]
    out = simplify(pts, TOLERANCE)
    if len(out) < 4:  # pierścień musi mieć co najmniej trójkąt + zamknięcie
        out = simplify(pts, TOLERANCE / 10)
    return [[round(x, PRECISION), round(y, PRECISION)] for x, y in out]


def polygon(el):
    ext = el.find(f"{GML}exterior/{GML}LinearRing/{GML}posList")
    rings = [ring(ext)]
    for inner in el.findall(f"{GML}interior/{GML}LinearRing/{GML}posList"):
        rings.append(ring(inner))
    return rings


def main(src, dst):
    feats = []
    for member in ET.parse(src).getroot().iter(f"{WFS}member"):
        unit = member[0]
        geom = unit.find(f"{MS}msGeometry")[0]
        if geom.tag == f"{GML}Polygon":
            polys = [polygon(geom)]
        else:
            polys = [polygon(p) for p in geom.iter(f"{GML}Polygon")]
        feats.append({
            "type": "Feature",
            "properties": {
                "teryt": unit.findtext(f"{MS}JPT_KOD_JE"),
                "nazwa": unit.findtext(f"{MS}JPT_NAZWA_"),
            },
            "geometry": {"type": "MultiPolygon", "coordinates": polys},
        })
    json.dump({"type": "FeatureCollection", "features": feats}, open(dst, "w"), ensure_ascii=False, separators=(",", ":"))
    print(len(feats), "powiatów")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
