# PR 28 long-duration histogram review response

- Extended merge-queue attempt-duration histogram resolution beyond seven days with explicit 30-day, 90-day, and 365-day buckets.
- Ensured every duration accepted by the inclusive 365-day sanity ceiling falls within a finite bucket.
- Added a regression test proving a 30-day attempt is represented in both the 30-day and 365-day cumulative buckets.
- Kept semantic webhook dispatch deferred as specified by issue 23.
