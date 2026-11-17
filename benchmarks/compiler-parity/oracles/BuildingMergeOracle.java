import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.onthegomap.planetiler.FeatureMerge;
import com.onthegomap.planetiler.VectorTile;
import com.onthegomap.planetiler.geo.GeoUtils;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.IdentityHashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import org.locationtech.jts.geom.Coordinate;
import org.locationtech.jts.geom.Geometry;
import org.locationtech.jts.geom.LinearRing;
import org.locationtech.jts.geom.Polygon;
import org.locationtech.jts.operation.buffer.BufferOp;
import org.locationtech.jts.operation.buffer.BufferParameters;

/** Replays an ordered native building group through pinned Planetiler's z13 merge. */
public class BuildingMergeOracle {
  private static LinearRing ring(JsonNode points) {
    Coordinate[] coordinates = new Coordinate[points.size()];
    for (int i = 0; i < points.size(); i++) {
      var point = points.get(i);
      coordinates[i] = new Coordinate(point.get("x").asDouble() / 16,
          point.get("y").asDouble() / 16);
    }
    return GeoUtils.JTS_FACTORY.createLinearRing(coordinates);
  }

  private static Geometry geometry(JsonNode groups) {
    Polygon[] polygons = new Polygon[groups.size()];
    for (int i = 0; i < groups.size(); i++) {
      var group = groups.get(i);
      var holes = group.get("holes");
      LinearRing[] interior = new LinearRing[holes.size()];
      for (int j = 0; j < holes.size(); j++) {
        interior[j] = ring(holes.get(j));
      }
      polygons[i] = GeoUtils.JTS_FACTORY.createPolygon(ring(group.get("exterior")), interior);
    }
    return polygons.length == 1 ? polygons[0] : GeoUtils.JTS_FACTORY.createMultiPolygon(polygons);
  }

  public static void main(String[] args) throws Exception {
    var mapper = new ObjectMapper();
    var input = mapper.readTree(Path.of(args[0]).toFile());
    List<VectorTile.Feature> features = new ArrayList<>();
    for (var record : input) {
      features.add(new VectorTile.Feature("building", record.get("id").asLong(),
          VectorTile.encodeGeometry(geometry(record.get("geometry").get("Polygon"))), Map.of()));
    }
    var decoded = features.stream().map(feature -> {
      try {
        return feature.geometry().decode();
      } catch (Exception e) {
        throw new RuntimeException(e);
      }
    }).toList();
    var index = new IdentityHashMap<Geometry, Integer>();
    for (int i = 0; i < decoded.size(); i++) {
      index.put(decoded.get(i), i);
    }
    var result = new ArrayList<Map<String, Object>>();
    for (var cluster : FeatureMerge.groupPolygonsByProximity(decoded, 0.5)) {
      var members = cluster.stream().map(index::get).toList();
      var subset = members.stream().map(features::get).toList();
      var merged = FeatureMerge.mergeNearbyPolygons(subset, 4, 4, 0.5, 0.5);
      var output = new ArrayList<Map<String, Object>>();
      for (var feature : merged) {
        var polygon = feature.geometry().decode();
        output.add(Map.of("id", feature.id(), "wkt", polygon.toText(), "area_px2", polygon.getArea()));
      }
      var row = new LinkedHashMap<String, Object>();
      row.put("members", members);
      row.put("source_ids", members.stream().map(i -> features.get(i).id()).toList());
      row.put("output", output);
      if (members.size() == 5 && subset.getFirst().id() == 619340532L) {
        var parameters = new BufferParameters();
        parameters.setJoinStyle(BufferParameters.JOIN_MITRE);
        var expanded = cluster.stream().map(shape ->
            new BufferOp(shape, parameters).getResultGeometry(0.5)).toList();
        var unioned = GeoUtils.createGeometryCollection(expanded).union();
        var contracted = new BufferOp(unioned, parameters).getResultGeometry(-0.5);
        var snapped = GeoUtils.snapAndFixPolygon(contracted, null, "building-stage-oracle").reverse();
        row.put("stages", Map.of(
            "expanded", expanded.stream().map(Geometry::toText).toList(),
            "unioned", unioned.toText(),
            "contracted", contracted.toText(),
            "snapped", snapped.toText()));
      }
      result.add(row);
    }
    mapper.writeValue(Path.of(args[1]).toFile(), result);
  }
}
