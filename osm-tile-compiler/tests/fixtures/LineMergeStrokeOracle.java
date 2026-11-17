import com.onthegomap.planetiler.util.LoopLineMerger;
import org.locationtech.jts.geom.Coordinate;
import org.locationtech.jts.geom.GeometryFactory;

/** Runs two small stroke-merger oracles against the pinned Planetiler 0.10.2 JAR. */
public class LineMergeStrokeOracle {
  private static void show(String name, int[][][] paths) {
    var factory = new GeometryFactory();
    var merger = new LoopLineMerger().setMergeStrokes(true).setTolerance(0);
    for (var path : paths) {
      var coords = new Coordinate[path.length];
      for (int i = 0; i < path.length; i++) {
        coords[i] = new Coordinate(path[i][0], path[i][1]);
      }
      merger.add(factory.createLineString(coords));
    }
    System.out.println(name);
    for (var line : merger.getMergedLineStrings()) {
      System.out.println(line.toText());
    }
  }

  public static void main(String[] args) {
    show("actual boundary feature 399309380 z5/17/10", new int[][][] {
      {{3152,3194},{3153,3201},{3157,3204},{3168,3205}},
      {{3168,3205},{3176,3198},{3175,3196},{3189,3199}},
      {{3168,3205},{3168,3211},{3172,3213},{3175,3223}}
    });
    show("synthetic shared-interior four-way junction", new int[][][] {
      {{0,0},{10,0},{20,0}}, {{10,0},{10,10}}, {{10,0},{10,-10}}
    });
  }
}
