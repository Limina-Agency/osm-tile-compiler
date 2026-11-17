import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.SerializationFeature;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.util.ArrayList;
import java.util.HexFormat;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import org.locationtech.jts.geom.Coordinate;
import org.locationtech.jts.geom.PrecisionModel;
import org.locationtech.jts.noding.NodedSegmentString;
import org.locationtech.jts.noding.ScaledNoder;
import org.locationtech.jts.noding.SegmentString;
import org.locationtech.jts.noding.snapround.SnapRoundingNoder;

/** Exact-bit replay of the captured landuse cluster's negative-buffer curves. */
public final class LanduseScaledNoderOracle {
  private static final double MVT_SCALE = 16.0;
  private static final double NODING_SCALE = 1_000_000_000.0;

  private record Trace(int sourceCurve, int left, int right) {}

  private static double pixelCoordinate(JsonNode hex) {
    double mvt = Double.longBitsToDouble(Long.parseUnsignedLong(hex.asText(), 16));
    double pixel = mvt / MVT_SCALE;
    if (!Double.isFinite(pixel)) {
      throw new IllegalArgumentException("nonfinite captured coordinate");
    }
    return pixel;
  }

  private static String bits(double pixel) {
    return String.format("%016x", Double.doubleToRawLongBits(pixel));
  }

  private static Map<String, Object> describe(SegmentString segment) {
    Trace trace = (Trace) segment.getData();
    List<List<String>> hex = new ArrayList<>();
    List<List<Long>> grid = new ArrayList<>();
    for (Coordinate point : segment.getCoordinates()) {
      hex.add(List.of(bits(point.x), bits(point.y)));
      grid.add(List.of(Math.round(point.x * NODING_SCALE), Math.round(point.y * NODING_SCALE)));
    }
    var result = new LinkedHashMap<String, Object>();
    result.put("source_curve", trace.sourceCurve());
    result.put("left_location", trace.left());
    result.put("right_location", trace.right());
    result.put("coordinates_pixel_ieee754_hex", hex);
    result.put("coordinates_scaled_i64", grid);
    return result;
  }

  public static void main(String[] args) throws Exception {
    if (args.length != 2) {
      throw new IllegalArgumentException("usage: LanduseScaledNoderOracle BITS_JSON OUTPUT_JSON");
    }
    Path inputPath = Path.of(args[0]);
    byte[] inputBytes = Files.readAllBytes(inputPath);
    var mapper = new ObjectMapper();
    mapper.configure(SerializationFeature.ORDER_MAP_ENTRIES_BY_KEYS, true);
    JsonNode capture = mapper.readTree(inputBytes);
    JsonNode rows = capture.get("negative_buffer_raw_curves");
    if (rows == null || rows.size() != 13) {
      throw new IllegalArgumentException("expected the captured 13 erosion curves");
    }
    var curves = new ArrayList<NodedSegmentString>();
    for (JsonNode row : rows) {
      JsonNode coordinates = row.get("coordinates");
      Coordinate[] points = new Coordinate[coordinates.size()];
      for (int i = 0; i < points.length; i++) {
        points[i] = new Coordinate(pixelCoordinate(coordinates.get(i).get(0)),
            pixelCoordinate(coordinates.get(i).get(1)));
      }
      curves.add(new NodedSegmentString(points,
          new Trace(row.get("source_curve").asInt(), row.get("left_location").asInt(),
              row.get("right_location").asInt())));
    }

    var inputCurves = curves.stream().map(LanduseScaledNoderOracle::describe).toList();
    var scaled = new ScaledNoder(new SnapRoundingNoder(new PrecisionModel(1)), NODING_SCALE);
    scaled.computeNodes(curves);
    var edges = new ArrayList<Map<String, Object>>();
    for (Object object : scaled.getNodedSubstrings()) {
      edges.add(describe((SegmentString) object));
    }
    var result = new LinkedHashMap<String, Object>();
    result.put("input_sha256", HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(inputBytes)));
    result.put("input_curve_count", curves.size());
    result.put("input_pixel_curves", inputCurves);
    result.put("noded_edge_count", edges.size());
    result.put("noded_edges", edges);
    result.put("coordinate_unit", "tile pixels (MVT ordinate divided by exactly 16)");
    result.put("coordinate_encoding", "IEEE-754 binary64 raw bits, 16-digit lowercase hex");
    result.put("noder", "ScaledNoder(SnapRoundingNoder(PrecisionModel(1)),1e9)");
    result.put("scaled_integer_grid", "Math.round(pixel*1e9), diagnostic projection of returned coordinates");
    mapper.writeValue(Path.of(args[1]).toFile(), result);
  }
}
