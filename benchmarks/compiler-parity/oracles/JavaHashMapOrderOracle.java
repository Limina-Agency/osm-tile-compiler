import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.stream.Collectors;
import java.util.stream.IntStream;

/**
 * Print JDK 21 HashMap iteration order for the four ways the pinned profile and
 * engine fill one, as comma-separated indices of each key's first occurrence.
 * The input file holds one key per line, as UTF-8 text or as decimal longs.
 *
 * <ul>
 *   <li>{@code grouping-string}: Collectors.groupingBy over strings (Housenumber.postProcess)</li>
 *   <li>{@code grouping-long}: Collectors.groupingBy over longs (Boundary mode)</li>
 *   <li>{@code put-sized-string}: HashMap.newHashMap(n), one put per distinct key (PBF tags)</li>
 *   <li>{@code hashset-long}: new HashSet&lt;Long&gt;(), one add per key (Boundary regions)</li>
 * </ul>
 */
public final class JavaHashMapOrderOracle {
  private JavaHashMapOrderOracle() {}

  public static void main(String[] args) throws Exception {
    if (args.length != 2) {
      throw new IllegalArgumentException("expected MODE FILE");
    }
    List<String> lines = Files.readAllLines(Path.of(args[1]), StandardCharsets.UTF_8);
    List<Integer> order = switch (args[0]) {
      case "grouping-string" -> grouping(lines);
      case "grouping-long" -> grouping(lines.stream().map(Long::parseLong).toList());
      case "put-sized-string" -> {
        Map<String, Integer> map = HashMap.newHashMap(lines.size());
        for (int i = 0; i < lines.size(); i++) {
          map.putIfAbsent(lines.get(i), i);
        }
        yield new ArrayList<>(map.values());
      }
      case "hashset-long" -> {
        Map<Long, Integer> first = new HashMap<>();
        HashSet<Long> set = new HashSet<>();
        for (int i = 0; i < lines.size(); i++) {
          long key = Long.parseLong(lines.get(i));
          set.add(key);
          first.putIfAbsent(key, i);
        }
        yield set.stream().map(first::get).toList();
      }
      default -> throw new IllegalArgumentException("unknown mode " + args[0]);
    };
    System.out.println(order.stream().map(String::valueOf).collect(Collectors.joining(",")));
  }

  private static <K> List<Integer> grouping(List<K> keys) {
    Map<K, List<Integer>> groups = IntStream.range(0, keys.size())
      .boxed()
      .collect(Collectors.groupingBy(keys::get));
    return groups.values().stream().map(List::getFirst).toList();
  }
}
