# reef-broker

The subscription broker of **Reef**, version control built for a world where AI agents write most of the code ([ADR 0038](../../docs/adr/0038-subscription-broker.md)). Between a log and the hundreds of streams that follow it, a producer reads each new entry once and appends it to a bounded in-memory ring per topic; readers serve their streams from the ring and go back to the log only for a position the ring no longer holds. A slow reader pins nothing: rings are evicted by their bounds, never held for the slowest.

It is generic over the topic (a repository), the position between entries (an operation-log frontier) and the item (a converted operation), and depends on nothing but `tokio`. The Reef daemon's operation-log subscriptions are its first producer and reader (`reef-daemon`'s `subscribe::broker`); the phase-four store reuses it with its own. The design document and roadmap are at <https://github.com/reef-vcs/reef>.
