pub fn parse_hunk_header(line: &str) -> Option<(usize, usize)> {
    let header = line.strip_prefix("@@ -")?;
    let (_, added) = header.split_once(" +")?;
    let added = added.split_once(" @@").map_or(added, |(span, _)| span);
    let (start, len) = added.split_once(',').unwrap_or((added, "1"));
    Some((start.parse().ok()?, len.parse().ok()?))
}
