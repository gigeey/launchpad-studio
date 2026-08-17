use std::path::Path;

use ao_protocol::error::AoError;
use ao_protocol::transcript::{CursorPhase, PaginatedResponse, PaginationCursor, TranscriptEntry};
use chrono::{DateTime, Utc};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use grep_searcher::Searcher;

use crate::paths::DataRoot;

/// JSONL-based transcript persistence.
/// Each agent has a `.jsonl` file with one JSON entry per line.
pub struct TranscriptStore {
    data_root: DataRoot,
}

impl TranscriptStore {
    pub fn new(data_root: DataRoot) -> Self {
        Self { data_root }
    }

    /// Append a transcript entry as a single-line JSON to the agent's JSONL file.
    pub async fn append(&self, agent_id: &str, entry: &TranscriptEntry) -> Result<(), AoError> {
        let path = self.data_root.agent_transcript_path(agent_id);

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let line = serde_json::to_string(entry).map_err(|e| AoError::Json(e.to_string()))?;
        let line_with_newline = format!("{}\n", line);

        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        file.write_all(line_with_newline.as_bytes()).await?;
        // `write_all` on a tokio `File` returns once the write is queued on the
        // blocking pool, not once the bytes have landed. Without this flush the
        // handle is dropped while the write may still be in flight, and `Drop`
        // discards whatever error the close reports — so a full disk or an I/O
        // error would surface as `Ok(())` here and a silently short file on
        // disk. Flushing both makes the entry readable to the next call and
        // gives the error somewhere to go.
        //
        // Deliberately `flush`, not `sync_all`: this makes the write visible to
        // every reader of the file, which is what callers actually depend on.
        // Surviving a machine-level power loss would need an fsync per appended
        // line, and these logs are written on hot paths; that cost was not
        // judged worth paying for an append-only diagnostic log. The same
        // pairing is used by every other append site in this crate.
        file.flush().await?;
        Ok(())
    }

    /// Read the last N transcript entries for an agent, newest last.
    /// Returns empty vec if file not found. Returns all entries if n > total count.
    /// Only parses the last N lines from the JSONL file, limiting memory for parsed output.
    pub async fn read_recent(&self, agent_id: &str, n: usize) -> Result<Vec<TranscriptEntry>, AoError> {
        let path = self.data_root.agent_transcript_path(agent_id);

        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(Vec::new());
        }

        let contents = tokio::fs::read_to_string(&path).await?;
        let lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();

        let start = lines.len().saturating_sub(n);
        let mut entries = Vec::with_capacity(lines.len() - start);
        for line in &lines[start..] {
            let entry: TranscriptEntry =
                serde_json::from_str(line).map_err(|e| AoError::Json(e.to_string()))?;
            entries.push(entry);
        }
        Ok(entries)
    }

    /// Search transcript entries by keyword (case-insensitive).
    /// Returns the most recent `limit` matching entries in chronological order.
    /// If query is empty, returns the last `limit` entries (equivalent to read_recent).
    pub async fn search(
        &self,
        agent_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<TranscriptEntry>, AoError> {
        let all = self.read_all(agent_id).await?;

        let filtered: Vec<TranscriptEntry> = if query.is_empty() {
            all
        } else {
            let query_lower = query.to_lowercase();
            all.into_iter()
                .filter(|entry| entry.content.to_lowercase().contains(&query_lower))
                .collect()
        };

        if limit >= filtered.len() {
            Ok(filtered)
        } else {
            Ok(filtered[filtered.len() - limit..].to_vec())
        }
    }

    /// Search transcript entries using grep-searcher for fast, streaming search.
    /// Matches against the content field of each JSONL line, case-insensitive.
    /// Returns the most recent `limit` matching entries in chronological order.
    pub async fn ripgrep_search(
        &self,
        agent_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<TranscriptEntry>, AoError> {
        let path = self.data_root.agent_transcript_path(agent_id);

        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(Vec::new());
        }

        let query = query.to_string();
        let path_clone = path.clone();

        let results = tokio::task::spawn_blocking(move || -> Result<Vec<TranscriptEntry>, AoError> {
            let matcher = RegexMatcherBuilder::new()
                .case_insensitive(true)
                .build(&regex::escape(&query))
                .map_err(|e| AoError::Internal(format!("regex error: {}", e)))?;

            let mut matching_entries: Vec<TranscriptEntry> = Vec::new();

            Searcher::new()
                .search_path(
                    &matcher,
                    &path_clone,
                    UTF8(|_line_num, line| {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            return Ok(true);
                        }
                        if let Ok(entry) = serde_json::from_str::<TranscriptEntry>(trimmed) {
                            // Verify the match is in the content field specifically
                            if entry.content.to_lowercase().contains(&query.to_lowercase()) {
                                matching_entries.push(entry);
                            }
                        }
                        Ok(true)
                    }),
                )
                .map_err(|e| AoError::Internal(format!("search error: {}", e)))?;

            // Return the most recent `limit` entries
            if limit >= matching_entries.len() {
                Ok(matching_entries)
            } else {
                Ok(matching_entries[matching_entries.len() - limit..].to_vec())
            }
        })
        .await
        .map_err(|e| AoError::Internal(format!("task join error: {}", e)))??;

        Ok(results)
    }

    /// Search across ALL agents' transcript files using grep-searcher.
    /// Returns (agent_id, TranscriptEntry) tuples for matching entries, up to `limit` most recent.
    pub async fn ripgrep_search_all(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(String, TranscriptEntry)>, AoError> {
        let data_dir = self.data_root.messages_data_dir();

        if !tokio::fs::try_exists(&data_dir).await.unwrap_or(false) {
            return Ok(Vec::new());
        }

        // Collect agent transcript files (exclude team_ prefixed and tasks/ subdirectory)
        let mut agent_ids: Vec<String> = Vec::new();
        let mut dir = tokio::fs::read_dir(&data_dir).await?;
        while let Some(entry) = dir.next_entry().await? {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            if name.ends_with(".jsonl") && !name.starts_with("team_") {
                let agent_id = name.trim_end_matches(".jsonl").to_string();
                agent_ids.push(agent_id);
            }
        }

        // Search each agent's transcript
        let mut all_results: Vec<(String, TranscriptEntry)> = Vec::new();
        for agent_id in &agent_ids {
            let entries = self.ripgrep_search(agent_id, query, limit).await?;
            for entry in entries {
                all_results.push((agent_id.clone(), entry));
            }
        }

        // Sort by timestamp descending (most recent first), then take limit
        all_results.sort_by(|a, b| b.1.ts.cmp(&a.1.ts));
        all_results.truncate(limit);

        Ok(all_results)
    }

    /// Read the last N transcript entries by seeking from EOF and reading backwards.
    /// Returns entries in chronological order (oldest first) with a cursor pointing
    /// to the byte offset of the oldest returned message. Cursor is None when the
    /// start of the file is reached.
    pub async fn read_tail(
        &self,
        agent_id: &str,
        n: usize,
    ) -> Result<PaginatedResponse<TranscriptEntry>, AoError> {
        let path = self.data_root.agent_transcript_path(agent_id);
        self.read_tail_at(&path, n).await
    }

    /// Path-addressed equivalent of [`Self::read_tail`]. Routes the same
    /// reverse-chunk scan + cursor build against any JSONL transcript file,
    /// so per-thread transcripts can paginate identically to the default.
    pub async fn read_tail_at(
        &self,
        path: &Path,
        n: usize,
    ) -> Result<PaginatedResponse<TranscriptEntry>, AoError> {
        if !tokio::fs::try_exists(path).await.unwrap_or(false) {
            return Ok(PaginatedResponse {
                entries: Vec::new(),
                cursor: None,
            });
        }

        use tokio::io::{AsyncReadExt, AsyncSeekExt};

        let mut file = tokio::fs::File::open(path).await?;
        let file_len = file.metadata().await?.len();

        if file_len == 0 {
            return Ok(PaginatedResponse {
                entries: Vec::new(),
                cursor: None,
            });
        }

        // Read backwards from EOF in chunks, collecting complete lines
        const CHUNK_SIZE: u64 = 8192;
        let mut remaining = file_len;
        let mut tail_buf: Vec<u8> = Vec::new();
        let mut line_offsets: Vec<u64> = Vec::new(); // byte offsets of line starts

        // We'll accumulate bytes from end-of-file and track line boundaries
        loop {
            let read_start = remaining.saturating_sub(CHUNK_SIZE);
            let read_len = (remaining - read_start) as usize;

            file.seek(std::io::SeekFrom::Start(read_start)).await?;
            let mut chunk = vec![0u8; read_len];
            file.read_exact(&mut chunk).await?;

            // Prepend chunk to our buffer
            tail_buf.splice(0..0, chunk.iter().cloned());

            // Recount all line starts in the accumulated buffer.
            // A line start is at position 0, or right after each '\n' that isn't the last byte.
            line_offsets.clear();
            line_offsets.push(0);
            for (i, &b) in tail_buf.iter().enumerate() {
                if b == b'\n' && i + 1 < tail_buf.len() {
                    line_offsets.push((i + 1) as u64);
                }
            }

            // We have enough lines, or we've reached the start of the file
            if line_offsets.len() > n || read_start == 0 {
                break;
            }

            remaining = read_start;
        }

        // Convert tail_buf line offsets to absolute file offsets
        let buf_start_in_file = file_len - tail_buf.len() as u64;

        // Determine which lines to take (the last N non-empty lines)
        let tail_str = String::from_utf8_lossy(&tail_buf);
        let mut all_lines: Vec<&str> = tail_str.lines().filter(|l| !l.trim().is_empty()).collect();

        // If the buffer doesn't start at the beginning of the file, the first
        // "line" is a fragment of a line that began before the buffer. Drop it.
        if buf_start_in_file > 0 && !all_lines.is_empty() {
            all_lines.remove(0);
        }

        // Map: for each non-empty line, find its byte offset in the buffer, then absolute
        let mut line_abs_offsets: Vec<u64> = Vec::new();
        {
            let mut search_from = 0usize;
            for line in &all_lines {
                // Find this line's start in tail_buf
                if let Some(pos) = tail_buf[search_from..]
                    .windows(line.len())
                    .position(|w| w == line.as_bytes())
                {
                    line_abs_offsets.push(buf_start_in_file + (search_from + pos) as u64);
                    search_from = search_from + pos + line.len();
                }
            }
        }

        let total_lines = all_lines.len();
        let start_idx = total_lines.saturating_sub(n);
        let selected_lines = &all_lines[start_idx..];
        let selected_offsets = &line_abs_offsets[start_idx..];

        // Parse entries
        let mut entries = Vec::with_capacity(selected_lines.len());
        for line in selected_lines {
            let entry: TranscriptEntry =
                serde_json::from_str(line).map_err(|e| AoError::Json(e.to_string()))?;
            entries.push(entry);
        }

        // Build cursor: points to the oldest returned message
        let cursor = if entries.is_empty() || (start_idx == 0 && buf_start_in_file == 0) {
            // We returned everything from the start of the file
            None
        } else {
            let oldest = &entries[0];
            Some(PaginationCursor {
                byte_offset: selected_offsets[0],
                last_message_id: oldest.ts.to_rfc3339(),
                timestamp: oldest.ts,
                phase: CursorPhase::Own,
            })
        };

        Ok(PaginatedResponse { entries, cursor })
    }

    /// Read N messages before a given cursor position, with validation and fallback.
    ///
    /// Happy path: seek to cursor.byte_offset, verify the line's ts matches cursor.last_message_id,
    /// then reverse-read N lines before that offset.
    ///
    /// Fallback 1: if line at offset doesn't match, scan +/- 4KB window for the matching message.
    /// Fallback 2: if window scan fails, binary search by timestamp (timestamps are monotonic).
    /// If message was deleted entirely, return N messages before cursor.timestamp.
    pub async fn read_before_cursor(
        &self,
        agent_id: &str,
        cursor: &PaginationCursor,
        n: usize,
    ) -> Result<PaginatedResponse<TranscriptEntry>, AoError> {
        let path = self.data_root.agent_transcript_path(agent_id);
        self.read_before_cursor_at(&path, cursor, n).await
    }

    /// Path-addressed equivalent of [`Self::read_before_cursor`]. Same
    /// happy-path / window-scan / binary-search fallback chain, against any
    /// JSONL transcript file.
    pub async fn read_before_cursor_at(
        &self,
        path: &Path,
        cursor: &PaginationCursor,
        n: usize,
    ) -> Result<PaginatedResponse<TranscriptEntry>, AoError> {
        if !tokio::fs::try_exists(path).await.unwrap_or(false) {
            return Ok(PaginatedResponse {
                entries: Vec::new(),
                cursor: None,
            });
        }

        let mut file = tokio::fs::File::open(path).await?;
        let file_len = file.metadata().await?.len();

        if file_len == 0 {
            return Ok(PaginatedResponse {
                entries: Vec::new(),
                cursor: None,
            });
        }

        // Try to find the anchor byte offset for the cursor's message
        let anchor_offset = self
            .find_anchor_offset(&mut file, file_len, cursor)
            .await?;

        // anchor_offset is the byte position of the start of the line we paginate before.
        // If anchor is 0, there's nothing before it.
        if anchor_offset == 0 {
            return Ok(PaginatedResponse {
                entries: Vec::new(),
                cursor: None,
            });
        }

        // Reverse-read N lines before anchor_offset
        self.reverse_read_n_lines_before(&mut file, file_len, anchor_offset, n)
            .await
    }

    /// Find the byte offset of the cursor's target message in the file.
    /// Tries: exact offset -> window scan -> binary search by timestamp.
    async fn find_anchor_offset(
        &self,
        file: &mut tokio::fs::File,
        file_len: u64,
        cursor: &PaginationCursor,
    ) -> Result<u64, AoError> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};

        // 1. Happy path: check line at cursor.byte_offset
        if cursor.byte_offset < file_len {
            if let Some(line) = self
                .read_line_at(file, file_len, cursor.byte_offset)
                .await?
            {
                if let Ok(entry) = serde_json::from_str::<TranscriptEntry>(&line) {
                    if entry.ts.to_rfc3339() == cursor.last_message_id {
                        return Ok(cursor.byte_offset);
                    }
                }
            }
        }

        // 2. Window scan: +/- 4KB around byte_offset.
        // A stale cursor can point past the current end of the file (e.g. after the
        // transcript was truncated or rotated, or carried over from a longer file).
        // Clamp both ends to [0, file_len] and keep scan_start <= scan_end so the
        // length can never underflow; when the cursor is beyond EOF the window
        // collapses to zero and we fall through to the timestamp binary search below.
        const WINDOW: u64 = 4096;
        let scan_end = cursor.byte_offset.saturating_add(WINDOW).min(file_len);
        let scan_start = cursor.byte_offset.saturating_sub(WINDOW).min(scan_end);
        let scan_len = (scan_end - scan_start) as usize;

        file.seek(std::io::SeekFrom::Start(scan_start)).await?;
        let mut buf = vec![0u8; scan_len];
        file.read_exact(&mut buf).await?;

        let text = String::from_utf8_lossy(&buf);
        let mut pos_in_buf: usize = 0;
        for line in text.lines() {
            if !line.trim().is_empty() {
                if let Ok(entry) = serde_json::from_str::<TranscriptEntry>(line) {
                    if entry.ts.to_rfc3339() == cursor.last_message_id {
                        // Find exact byte position of this line in the buffer
                        if let Some(found) = buf[pos_in_buf..]
                            .windows(line.len())
                            .position(|w| w == line.as_bytes())
                        {
                            return Ok(scan_start + (pos_in_buf + found) as u64);
                        }
                    }
                }
            }
            // Advance past this line + newline
            if let Some(found) = buf[pos_in_buf..]
                .windows(line.len())
                .position(|w| w == line.as_bytes())
            {
                pos_in_buf = pos_in_buf + found + line.len();
                // Skip newline character(s)
                while pos_in_buf < buf.len() && (buf[pos_in_buf] == b'\n' || buf[pos_in_buf] == b'\r')
                {
                    pos_in_buf += 1;
                }
            }
        }

        // 3. Binary search by timestamp
        self.binary_search_by_timestamp(file, file_len, &cursor.timestamp)
            .await
    }

    /// Read a single complete line starting at the given byte offset.
    /// Returns None if offset is at or past EOF.
    async fn read_line_at(
        &self,
        file: &mut tokio::fs::File,
        file_len: u64,
        offset: u64,
    ) -> Result<Option<String>, AoError> {
        if offset >= file_len {
            return Ok(None);
        }

        use tokio::io::{AsyncReadExt, AsyncSeekExt};

        file.seek(std::io::SeekFrom::Start(offset)).await?;
        // Read up to 64KB for a single line (should be more than enough)
        let read_len = ((file_len - offset) as usize).min(65536);
        let mut buf = vec![0u8; read_len];
        file.read_exact(&mut buf).await?;

        let text = String::from_utf8_lossy(&buf);
        if let Some(line) = text.lines().next() {
            if !line.trim().is_empty() {
                return Ok(Some(line.to_string()));
            }
        }
        Ok(None)
    }

    /// Binary search for the byte offset of the message closest to (but <= ) the target timestamp.
    /// Since timestamps are monotonically increasing in the JSONL, we can bisect.
    async fn binary_search_by_timestamp(
        &self,
        file: &mut tokio::fs::File,
        file_len: u64,
        target_ts: &DateTime<Utc>,
    ) -> Result<u64, AoError> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};

        let mut lo: u64 = 0;
        let mut hi: u64 = file_len;
        let mut best_offset: u64 = 0;

        while lo < hi {
            let mid = lo + (hi - lo) / 2;

            // Seek to one byte before `mid` (when mid > 0), not to `mid`
            // itself. This lets us tell whether `mid` lands exactly on a
            // line's start byte: in that case the leading byte we read back
            // is the previous line's trailing '\n', so `.lines()` yields an
            // empty first element for it and the *real* line beginning at
            // `mid` survives as the next element. Seeking to `mid` directly
            // couldn't distinguish "mid is mid-line" from "mid is itself a
            // line start" - both looked like "the first parsed line might be
            // partial" and were unconditionally discarded, so a `mid` that
            // landed exactly on an entry's own start byte skipped past that
            // entry and searched only the right half, one entry too far.
            let seek_pos = mid.saturating_sub(1);
            file.seek(std::io::SeekFrom::Start(seek_pos)).await?;
            let read_len = ((file_len - seek_pos) as usize).min(65536);
            let mut buf = vec![0u8; read_len];
            let bytes_read = file.read(&mut buf).await?;
            buf.truncate(bytes_read);

            let text = String::from_utf8_lossy(&buf);
            let mut lines = text.lines();

            // If mid > 0, skip the first (partial-or-empty) line
            if mid > 0 {
                lines.next();
            }

            if let Some(line) = lines.next() {
                if !line.trim().is_empty() {
                    if let Ok(entry) = serde_json::from_str::<TranscriptEntry>(line) {
                        if entry.ts <= *target_ts {
                            // Find the absolute offset of this line
                            let line_rel = text.find(line).unwrap_or(0);
                            let mut abs = seek_pos + line_rel as u64;
                            // If we skipped a partial line, adjust
                            if mid > 0 {
                                // The line starts after the first \n from seek_pos
                                if let Some(nl_pos) = buf.iter().position(|&b| b == b'\n') {
                                    abs = seek_pos + nl_pos as u64 + 1;
                                }
                            }
                            best_offset = abs;
                            lo = abs + line.len() as u64 + 1; // +1 for newline
                            continue;
                        } else {
                            hi = mid;
                            continue;
                        }
                    }
                }
            }

            // Couldn't parse a line here, narrow the search
            if mid == lo {
                break;
            }
            hi = mid;
        }

        Ok(best_offset)
    }

    /// Reverse-read N lines from the file before the given anchor byte offset.
    /// Returns entries in chronological order with appropriate cursor.
    async fn reverse_read_n_lines_before(
        &self,
        file: &mut tokio::fs::File,
        _file_len: u64,
        anchor_offset: u64,
        n: usize,
    ) -> Result<PaginatedResponse<TranscriptEntry>, AoError> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};

        if anchor_offset == 0 {
            return Ok(PaginatedResponse {
                entries: Vec::new(),
                cursor: None,
            });
        }

        // Read backwards from anchor_offset in chunks
        const CHUNK_SIZE: u64 = 8192;
        let mut remaining = anchor_offset;
        let mut buf: Vec<u8> = Vec::new();

        loop {
            let read_start = remaining.saturating_sub(CHUNK_SIZE);
            let read_len = (remaining - read_start) as usize;

            file.seek(std::io::SeekFrom::Start(read_start)).await?;
            let mut chunk = vec![0u8; read_len];
            file.read_exact(&mut chunk).await?;

            buf.splice(0..0, chunk.iter().cloned());

            // Count non-empty lines
            let text = String::from_utf8_lossy(&buf);
            let line_count = text.lines().filter(|l| !l.trim().is_empty()).count();

            if line_count >= n || read_start == 0 {
                break;
            }

            remaining = read_start;
        }

        let buf_start_in_file = anchor_offset - buf.len() as u64;
        let text = String::from_utf8_lossy(&buf);
        let mut all_lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();

        // If the buffer doesn't start at the beginning of the file, the first
        // "line" is a fragment of a line that began before the buffer. Drop it.
        if buf_start_in_file > 0 && !all_lines.is_empty() {
            all_lines.remove(0);
        }

        let total = all_lines.len();
        let start_idx = total.saturating_sub(n);
        let selected = &all_lines[start_idx..];

        // Find byte offsets for selected lines
        let mut line_abs_offsets: Vec<u64> = Vec::new();
        {
            let mut search_from = 0usize;
            for line in &all_lines {
                if let Some(pos) = buf[search_from..]
                    .windows(line.len())
                    .position(|w| w == line.as_bytes())
                {
                    line_abs_offsets.push(buf_start_in_file + (search_from + pos) as u64);
                    search_from = search_from + pos + line.len();
                }
            }
        }
        let selected_offsets = &line_abs_offsets[start_idx..];

        let mut entries = Vec::with_capacity(selected.len());
        for line in selected {
            let entry: TranscriptEntry =
                serde_json::from_str(line).map_err(|e| AoError::Json(e.to_string()))?;
            entries.push(entry);
        }

        let cursor = if entries.is_empty() || (start_idx == 0 && buf_start_in_file == 0) {
            None
        } else {
            let oldest = &entries[0];
            Some(PaginationCursor {
                byte_offset: selected_offsets[0],
                last_message_id: oldest.ts.to_rfc3339(),
                timestamp: oldest.ts,
                phase: CursorPhase::Own,
            })
        };

        Ok(PaginatedResponse { entries, cursor })
    }

    /// Read the last N entries at or before `floor` from `path`, with a
    /// cursor addressed against `path` and tagged `CursorPhase::Inherited`.
    ///
    /// Used for branch threads: `path` is the SOURCE thread's transcript,
    /// and `floor` is the fork timestamp (`Thread::history_floor_ts`).
    /// Entries appended to the source AFTER the fork must never surface in
    /// the branch's view, so this is NOT a plain tail read — it's the tail
    /// of the *inheritable prefix* only.
    ///
    /// Entries are appended in non-decreasing `ts` order, so "ts <= floor"
    /// is always a contiguous prefix of the file; this reads the whole file
    /// once, stops at the first post-floor line, and takes the tail of what
    /// it collected. Simpler (and, for typical transcript sizes, cheap
    /// enough) than extending the byte-offset binary search in
    /// [`Self::read_before_cursor_at`] with floor-awareness — that function
    /// is only ever asked to page further backward from an anchor already
    /// inside the inheritable prefix, where no floor check is needed (see
    /// callers in `routes/messages.rs`).
    pub async fn read_tail_before_floor_at(
        &self,
        path: &Path,
        floor: DateTime<Utc>,
        n: usize,
    ) -> Result<PaginatedResponse<TranscriptEntry>, AoError> {
        if !tokio::fs::try_exists(path).await.unwrap_or(false) {
            return Ok(PaginatedResponse {
                entries: Vec::new(),
                cursor: None,
            });
        }

        let contents = tokio::fs::read_to_string(path).await?;
        let mut offset: u64 = 0;
        let mut filtered: Vec<(u64, TranscriptEntry)> = Vec::new();
        for line in contents.lines() {
            let line_len = line.len() as u64;
            if !line.trim().is_empty() {
                match serde_json::from_str::<TranscriptEntry>(line) {
                    Ok(entry) if entry.ts <= floor => filtered.push((offset, entry)),
                    // Monotonic ts: the first post-floor line ends the
                    // inheritable prefix, nothing further can qualify.
                    Ok(_) => break,
                    Err(_) => {}
                }
            }
            offset += line_len + 1; // '+1' accounts for the stripped '\n'
        }

        let total = filtered.len();
        let start = total.saturating_sub(n);
        let selected = &filtered[start..];

        let entries: Vec<TranscriptEntry> = selected.iter().map(|(_, e)| e.clone()).collect();
        let cursor = if entries.is_empty() || start == 0 {
            None
        } else {
            let (oldest_offset, oldest_entry) = &selected[0];
            Some(PaginationCursor {
                byte_offset: *oldest_offset,
                last_message_id: oldest_entry.ts.to_rfc3339(),
                timestamp: oldest_entry.ts,
                phase: CursorPhase::Inherited,
            })
        };

        Ok(PaginatedResponse { entries, cursor })
    }

    /// Get the current byte size of an agent's transcript file.
    /// Returns 0 if the file does not exist.
    pub async fn file_size(&self, agent_id: &str) -> Result<u64, AoError> {
        let path = self.data_root.agent_transcript_path(agent_id);
        match tokio::fs::metadata(&path).await {
            Ok(meta) => Ok(meta.len()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(AoError::Io(e)),
        }
    }

    /// Truncate an agent's transcript file to a given byte size.
    /// Used to roll back partial writes (e.g. on run cancellation).
    pub async fn truncate_to_size(&self, agent_id: &str, size: u64) -> Result<(), AoError> {
        let path = self.data_root.agent_transcript_path(agent_id);
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(());
        }
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .await?;
        file.set_len(size).await?;
        Ok(())
    }

    /// Read all transcript entries for an agent. Returns empty vec if file not found.
    pub async fn read_all(&self, agent_id: &str) -> Result<Vec<TranscriptEntry>, AoError> {
        let path = self.data_root.agent_transcript_path(agent_id);

        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(Vec::new());
        }

        let contents = tokio::fs::read_to_string(&path).await?;
        let mut entries = Vec::new();
        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let entry: TranscriptEntry =
                serde_json::from_str(line).map_err(|e| AoError::Json(e.to_string()))?;
            entries.push(entry);
        }
        Ok(entries)
    }

    /// Append a transcript entry to an arbitrary path (creates parent dirs).
    /// Used by tasklist mode where transcripts live outside `messages_data_dir`.
    pub async fn append_at(&self, path: &Path, entry: &TranscriptEntry) -> Result<(), AoError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let line = serde_json::to_string(entry).map_err(|e| AoError::Json(e.to_string()))?;
        let line_with_newline = format!("{}\n", line);

        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        file.write_all(line_with_newline.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    /// Path-based equivalent of [`Self::read_recent`].
    pub async fn read_recent_at(
        &self,
        path: &Path,
        n: usize,
    ) -> Result<Vec<TranscriptEntry>, AoError> {
        if !tokio::fs::try_exists(path).await.unwrap_or(false) {
            return Ok(Vec::new());
        }
        let contents = tokio::fs::read_to_string(path).await?;
        let lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();
        let start = lines.len().saturating_sub(n);
        let mut entries = Vec::with_capacity(lines.len() - start);
        for line in &lines[start..] {
            let entry: TranscriptEntry =
                serde_json::from_str(line).map_err(|e| AoError::Json(e.to_string()))?;
            entries.push(entry);
        }
        Ok(entries)
    }

    /// Path-based equivalent of [`Self::read_all`].
    pub async fn read_all_at(&self, path: &Path) -> Result<Vec<TranscriptEntry>, AoError> {
        if !tokio::fs::try_exists(path).await.unwrap_or(false) {
            return Ok(Vec::new());
        }
        let contents = tokio::fs::read_to_string(path).await?;
        let mut entries = Vec::new();
        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let entry: TranscriptEntry =
                serde_json::from_str(line).map_err(|e| AoError::Json(e.to_string()))?;
            entries.push(entry);
        }
        Ok(entries)
    }

    /// Path-based equivalent of [`Self::file_size`].
    pub async fn file_size_at(&self, path: &Path) -> Result<u64, AoError> {
        match tokio::fs::metadata(path).await {
            Ok(meta) => Ok(meta.len()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(AoError::Io(e)),
        }
    }

    /// Path-based equivalent of [`Self::truncate_to_size`].
    pub async fn truncate_to_size_at(&self, path: &Path, size: u64) -> Result<(), AoError> {
        if !tokio::fs::try_exists(path).await.unwrap_or(false) {
            return Ok(());
        }
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .await?;
        file.set_len(size).await?;
        Ok(())
    }

    /// Path-based equivalent of [`Self::search`].
    pub async fn search_at(
        &self,
        path: &Path,
        query: &str,
        limit: usize,
    ) -> Result<Vec<TranscriptEntry>, AoError> {
        let all = self.read_all_at(path).await?;
        let filtered: Vec<TranscriptEntry> = if query.is_empty() {
            all
        } else {
            let query_lower = query.to_lowercase();
            all.into_iter()
                .filter(|entry| entry.content.to_lowercase().contains(&query_lower))
                .collect()
        };
        if limit >= filtered.len() {
            Ok(filtered)
        } else {
            Ok(filtered[filtered.len() - limit..].to_vec())
        }
    }

    // -- "for_run" helpers: dispatch to either the agent_id-keyed default path
    // -- or an explicit override (used by tasklist mode). Lets call sites in
    // -- agent_runner stay one-liner without branching.

    pub async fn append_for_run(
        &self,
        agent_id: &str,
        override_path: Option<&Path>,
        entry: &TranscriptEntry,
    ) -> Result<(), AoError> {
        match override_path {
            Some(p) => self.append_at(p, entry).await,
            None => self.append(agent_id, entry).await,
        }
    }

    pub async fn read_recent_for_run(
        &self,
        agent_id: &str,
        override_path: Option<&Path>,
        n: usize,
    ) -> Result<Vec<TranscriptEntry>, AoError> {
        match override_path {
            Some(p) => self.read_recent_at(p, n).await,
            None => self.read_recent(agent_id, n).await,
        }
    }

    pub async fn read_all_for_run(
        &self,
        agent_id: &str,
        override_path: Option<&Path>,
    ) -> Result<Vec<TranscriptEntry>, AoError> {
        match override_path {
            Some(p) => self.read_all_at(p).await,
            None => self.read_all(agent_id).await,
        }
    }

    pub async fn file_size_for_run(
        &self,
        agent_id: &str,
        override_path: Option<&Path>,
    ) -> Result<u64, AoError> {
        match override_path {
            Some(p) => self.file_size_at(p).await,
            None => self.file_size(agent_id).await,
        }
    }

    pub async fn truncate_to_size_for_run(
        &self,
        agent_id: &str,
        override_path: Option<&Path>,
        size: u64,
    ) -> Result<(), AoError> {
        match override_path {
            Some(p) => self.truncate_to_size_at(p, size).await,
            None => self.truncate_to_size(agent_id, size).await,
        }
    }

    pub async fn search_for_run(
        &self,
        agent_id: &str,
        override_path: Option<&Path>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<TranscriptEntry>, AoError> {
        match override_path {
            Some(p) => self.search_at(p, query, limit).await,
            None => self.search(agent_id, query, limit).await,
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use ao_protocol::transcript::TranscriptRole;
    use chrono::{Duration, Utc};
    use std::collections::HashMap;

    fn make_entry(i: usize) -> TranscriptEntry {
        TranscriptEntry {
            ts: Utc::now() - Duration::seconds(1000 - i as i64),
            role: TranscriptRole::System("user".to_string()),
            content: format!("message {}", i),
            event_type: "message".to_string(),
            metadata: Some(HashMap::new()),
            hidden_from_user: false,
        }
    }

    #[tokio::test]
    async fn append_for_run_with_override_writes_to_path_not_default() {
        // for_run helpers route to the per-tasklist transcript when an
        // override is provided, leaving the agent's default file untouched.
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = TranscriptStore::new(data_root.clone());

        let override_path =
            data_root.tasklist_agent_transcript_path("alpha", "tl-1", "agent-x");
        let entry = make_entry(1);
        store
            .append_for_run("agent-x", Some(&override_path), &entry)
            .await
            .unwrap();

        // Override file received the entry.
        let from_override = store.read_all_at(&override_path).await.unwrap();
        assert_eq!(from_override.len(), 1);
        // Default agent file is untouched.
        let from_default = store.read_all("agent-x").await.unwrap();
        assert!(from_default.is_empty());

        // Without override, writes go to the agent's default file.
        store
            .append_for_run("agent-x", None, &make_entry(2))
            .await
            .unwrap();
        let from_default = store.read_all("agent-x").await.unwrap();
        assert_eq!(from_default.len(), 1);
        // Override file is not affected by the second (no-override) write.
        let from_override = store.read_all_at(&override_path).await.unwrap();
        assert_eq!(from_override.len(), 1);
    }

    #[tokio::test]
    async fn for_run_helpers_route_reads_writes_size_and_truncate() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = TranscriptStore::new(data_root.clone());

        let path_a = data_root.tasklist_agent_transcript_path("alpha", "tl-A", "agent");
        let path_b = data_root.tasklist_agent_transcript_path("alpha", "tl-B", "agent");

        for i in 0..3 {
            store
                .append_for_run("agent", Some(&path_a), &make_entry(i))
                .await
                .unwrap();
        }
        store
            .append_for_run("agent", Some(&path_b), &make_entry(99))
            .await
            .unwrap();

        // Reads are properly routed by override.
        let recent_a = store
            .read_recent_for_run("agent", Some(&path_a), 10)
            .await
            .unwrap();
        let recent_b = store
            .read_recent_for_run("agent", Some(&path_b), 10)
            .await
            .unwrap();
        assert_eq!(recent_a.len(), 3);
        assert_eq!(recent_b.len(), 1);

        // Switching tasklists for the same agent yields disjoint history.
        assert_ne!(
            recent_a[0].content, recent_b[0].content,
            "tasklist A and B must not leak entries"
        );

        // file_size_for_run + truncate_to_size_for_run round-trip on the override.
        let size_a = store
            .file_size_for_run("agent", Some(&path_a), )
            .await
            .unwrap();
        assert!(size_a > 0);
        store
            .truncate_to_size_for_run("agent", Some(&path_a), 0)
            .await
            .unwrap();
        let after = store
            .read_all_for_run("agent", Some(&path_a))
            .await
            .unwrap();
        assert!(after.is_empty(), "truncate_to_size_for_run should empty the override file");

        // tasklist B is untouched.
        let recent_b_after = store
            .read_recent_for_run("agent", Some(&path_b), 10)
            .await
            .unwrap();
        assert_eq!(recent_b_after.len(), 1);
    }

    #[tokio::test]
    async fn test_read_tail_returns_last_n_with_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = TranscriptStore::new(data_root);

        // Write 100 entries
        let mut entries = Vec::new();
        for i in 0..100 {
            let entry = make_entry(i);
            store.append("test-agent", &entry).await.unwrap();
            entries.push(entry);
        }

        // Read last 50
        let result = store.read_tail("test-agent", 50).await.unwrap();

        assert_eq!(result.entries.len(), 50);

        // Should be entries 50..100 (the last 50), in chronological order
        for (i, entry) in result.entries.iter().enumerate() {
            assert_eq!(entry.content, format!("message {}", 50 + i));
        }

        // Cursor should be present (didn't reach start of file)
        let cursor = result.cursor.expect("cursor should be Some");
        assert_eq!(cursor.timestamp, result.entries[0].ts);
        assert_eq!(cursor.last_message_id, result.entries[0].ts.to_rfc3339());
        assert!(cursor.byte_offset > 0);
    }

    #[tokio::test]
    async fn test_read_tail_fewer_than_n_returns_all_no_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = TranscriptStore::new(data_root);

        // Write 10 entries
        for i in 0..10 {
            store.append("test-agent", &make_entry(i)).await.unwrap();
        }

        // Request 50, but only 10 exist
        let result = store.read_tail("test-agent", 50).await.unwrap();

        assert_eq!(result.entries.len(), 10);
        assert!(result.cursor.is_none(), "cursor should be None when all messages returned");

        // Verify chronological order
        for (i, entry) in result.entries.iter().enumerate() {
            assert_eq!(entry.content, format!("message {}", i));
        }
    }

    #[tokio::test]
    async fn test_read_tail_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = TranscriptStore::new(data_root);

        let result = store.read_tail("nonexistent-agent", 50).await.unwrap();
        assert!(result.entries.is_empty());
        assert!(result.cursor.is_none());
    }

    #[tokio::test]
    async fn test_read_before_cursor_happy_path() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = TranscriptStore::new(data_root);

        // Write 100 entries
        let mut entries = Vec::new();
        for i in 0..100 {
            let entry = make_entry(i);
            store.append("test-agent", &entry).await.unwrap();
            entries.push(entry);
        }

        // Get the last 50 (entries 50..100), which gives us a cursor pointing to entry 50
        let tail = store.read_tail("test-agent", 50).await.unwrap();
        let cursor = tail.cursor.expect("should have cursor");

        // Now read 50 before that cursor (should get entries 0..50)
        let before = store
            .read_before_cursor("test-agent", &cursor, 50)
            .await
            .unwrap();

        assert_eq!(before.entries.len(), 50);
        for (i, entry) in before.entries.iter().enumerate() {
            assert_eq!(entry.content, format!("message {}", i));
        }

        // Cursor should be None since we reached start of file
        assert!(
            before.cursor.is_none(),
            "cursor should be None when start of file reached"
        );
    }

    #[tokio::test]
    async fn test_read_before_cursor_deletion_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = TranscriptStore::new(data_root);

        // Write 100 entries
        let mut entries = Vec::new();
        for i in 0..100 {
            let entry = make_entry(i);
            store.append("test-agent", &entry).await.unwrap();
            entries.push(entry);
        }

        // Get cursor pointing to entry 50
        let tail = store.read_tail("test-agent", 50).await.unwrap();
        let cursor = tail.cursor.expect("should have cursor");

        // Delete a line from the middle of the file by rewriting without entry 25
        let path = store.data_root.agent_transcript_path("test-agent");
        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        let new_contents: String = lines
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 25)
            .map(|(_, l)| format!("{}\n", l))
            .collect();
        tokio::fs::write(&path, &new_contents).await.unwrap();

        // The cursor byte_offset is now shifted, but the message at entry 50 still exists.
        // The window scan or binary search should find it.
        let before = store
            .read_before_cursor("test-agent", &cursor, 20)
            .await
            .unwrap();

        // We should get messages before entry 50
        assert!(!before.entries.is_empty());
        assert!(before.entries.len() <= 20);

        // All entries should be chronologically before entry 50
        for entry in &before.entries {
            assert!(entry.ts < entries[50].ts);
        }

        // Entries should be in chronological order
        for i in 1..before.entries.len() {
            assert!(before.entries[i].ts >= before.entries[i - 1].ts);
        }
    }

    #[tokio::test]
    async fn test_read_before_cursor_stale_offset_past_eof_falls_back_to_binary_search() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = TranscriptStore::new(data_root);

        // Write 100 entries
        let mut entries = Vec::new();
        for i in 0..100 {
            let entry = make_entry(i);
            store.append("test-agent", &entry).await.unwrap();
            entries.push(entry);
        }

        // Get a valid cursor pointing at entry 50, then corrupt its
        // byte_offset to point WELL past the current end of the file (as if
        // it were carried over from a longer file, or minted before a
        // truncation/rotation). last_message_id/timestamp still match a real
        // entry, so the window scan should collapse to zero-length and the
        // lookup should fall through to the timestamp binary search instead
        // of underflowing `scan_end - scan_start` and panicking.
        let tail = store.read_tail("test-agent", 50).await.unwrap();
        let valid_cursor = tail.cursor.expect("should have cursor");
        let file_len = store.file_size("test-agent").await.unwrap();
        let stale_cursor = PaginationCursor {
            byte_offset: file_len + 100_000,
            ..valid_cursor
        };

        // The key assertion is that this resolves at all instead of
        // panicking. The exact entry count returned isn't pinned here: the
        // timestamp binary search fallback has its own pre-existing
        // off-by-one on exact-timestamp ties, independent of this fix.
        let before = store
            .read_before_cursor("test-agent", &stale_cursor, 50)
            .await
            .unwrap();

        assert!(!before.entries.is_empty());
        assert!(before.entries.len() <= 50);
        for entry in &before.entries {
            assert!(entry.ts < entries[50].ts);
        }
        for i in 1..before.entries.len() {
            assert!(before.entries[i].ts >= before.entries[i - 1].ts);
        }
    }

    #[tokio::test]
    async fn test_binary_search_exact_timestamp_match_returns_owning_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = TranscriptStore::new(data_root);

        // Write 100 entries
        let mut entries = Vec::new();
        for i in 0..100 {
            let entry = make_entry(i);
            store.append("test-agent", &entry).await.unwrap();
            entries.push(entry);
        }

        // Cursor pointing at entry 50: its timestamp is an EXACT copy of
        // entries[50].ts, which is the common case for real pagination
        // cursors (not a synthetic edge case).
        let tail = store.read_tail("test-agent", 50).await.unwrap();
        let valid_cursor = tail.cursor.expect("should have cursor");
        assert_eq!(valid_cursor.timestamp, entries[50].ts);

        // Force both the happy path (byte_offset match) and the window
        // scan to miss, so find_anchor_offset falls all the way through to
        // the timestamp binary search - same technique as
        // test_read_before_cursor_stale_offset_past_eof_falls_back_to_binary_search.
        let file_len = store.file_size("test-agent").await.unwrap();
        let stale_cursor = PaginationCursor {
            byte_offset: file_len + 100_000,
            ..valid_cursor
        };

        let before = store
            .read_before_cursor("test-agent", &stale_cursor, 50)
            .await
            .unwrap();

        // The binary search must anchor on entry 50 itself (the entry that
        // OWNS the exact timestamp), so "before" returns exactly entries
        // 0..50 - including entry 49, which the off-by-one used to drop
        // silently by anchoring one entry early (on entry 49's own offset
        // instead of entry 50's).
        assert_eq!(before.entries.len(), 50);
        for (i, entry) in before.entries.iter().enumerate() {
            assert_eq!(entry.content, format!("message {}", i));
        }
    }

    #[tokio::test]
    async fn test_read_before_cursor_start_of_file() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = TranscriptStore::new(data_root);

        // Write 10 entries
        for i in 0..10 {
            store.append("test-agent", &make_entry(i)).await.unwrap();
        }

        // Get cursor pointing to entry 5
        let tail = store.read_tail("test-agent", 5).await.unwrap();
        let cursor = tail.cursor.expect("should have cursor");

        // Read 100 before cursor (only 5 exist before it)
        let before = store
            .read_before_cursor("test-agent", &cursor, 100)
            .await
            .unwrap();

        assert_eq!(before.entries.len(), 5);
        assert!(
            before.cursor.is_none(),
            "cursor should be None at start of file"
        );

        for (i, entry) in before.entries.iter().enumerate() {
            assert_eq!(entry.content, format!("message {}", i));
        }
    }

    #[tokio::test]
    async fn test_read_tail_before_floor_returns_only_pre_floor_entries_with_inherited_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = TranscriptStore::new(data_root.clone());
        let path = data_root.agent_transcript_path("source-agent");

        // Write 20 entries; the floor sits at entry 14, so 0..=14 (15
        // entries) are "inheritable" and 15..20 are post-fork writes that
        // must never surface via this method.
        let mut entries = Vec::new();
        for i in 0..20 {
            let entry = make_entry(i);
            store.append_at(&path, &entry).await.unwrap();
            entries.push(entry);
        }
        let floor = entries[14].ts;

        let result = store
            .read_tail_before_floor_at(&path, floor, 5)
            .await
            .unwrap();
        assert_eq!(result.entries.len(), 5);
        for (i, entry) in result.entries.iter().enumerate() {
            assert_eq!(entry.content, format!("message {}", 10 + i));
        }
        for entry in &result.entries {
            assert!(entry.ts <= floor, "must never return a post-floor entry");
        }

        let cursor = result
            .cursor
            .expect("cursor should be Some — more pre-floor history remains");
        assert_eq!(cursor.phase, CursorPhase::Inherited);
        // Cursor addresses the OLDEST entry in the just-returned page
        // (entries[10..15] was returned), matching read_tail_at's
        // convention so "before this cursor" is exclusive of entries[10].
        assert_eq!(cursor.timestamp, entries[10].ts);

        // Paginating "before" this cursor via the ordinary reverse-read
        // walks further back through the SAME file with no floor re-check
        // needed — moving strictly backward from an anchor already inside
        // the inheritable prefix can never re-cross the floor.
        let before = store
            .read_before_cursor_at(&path, &cursor, 100)
            .await
            .unwrap();
        assert_eq!(before.entries.len(), 10);
        assert!(before.cursor.is_none());
        for (i, entry) in before.entries.iter().enumerate() {
            assert_eq!(entry.content, format!("message {}", i));
        }
    }

    #[tokio::test]
    async fn test_read_tail_before_floor_all_pre_floor_entries_fit_returns_no_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = TranscriptStore::new(data_root.clone());
        let path = data_root.agent_transcript_path("source-agent");

        let mut entries = Vec::new();
        for i in 0..10 {
            let entry = make_entry(i);
            store.append_at(&path, &entry).await.unwrap();
            entries.push(entry);
        }
        // Floor after entry 4: only entries 0..=4 qualify, and all 5 fit in
        // one page, so there's nothing further to paginate into.
        let floor = entries[4].ts;

        let result = store
            .read_tail_before_floor_at(&path, floor, 50)
            .await
            .unwrap();
        assert_eq!(result.entries.len(), 5);
        assert!(
            result.cursor.is_none(),
            "all pre-floor entries fit in one page"
        );
        for entry in &result.entries {
            assert!(entry.ts <= floor);
        }
    }

    #[tokio::test]
    async fn test_read_tail_before_floor_empty_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        let store = TranscriptStore::new(data_root.clone());
        let path = data_root.agent_transcript_path("nonexistent-source");

        let result = store
            .read_tail_before_floor_at(&path, Utc::now(), 50)
            .await
            .unwrap();
        assert!(result.entries.is_empty());
        assert!(result.cursor.is_none());
    }
}
