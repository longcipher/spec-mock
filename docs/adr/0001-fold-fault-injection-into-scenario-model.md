# Fold fault injection into the scenario model, keeping `Prefer: code=`

An error response becomes a transition into a state whose declared response is an
error, rather than a separate mechanism. The `Prefer: code=<status>` header stays
supported and documented, but is defined as a shortcut that bypasses the scenario
machine rather than as a peer of it.

We chose this over keeping the two mechanisms orthogonal, because a scenario author
should not need two mental models to answer "why did I get a 404" — under the
orthogonal design the answer is sometimes the current state and sometimes a header
the test happened to send. We rejected deprecating the header outright: it is a
documented part of the public surface, and scenario-driven errors are strictly less
convenient for a one-off "I want to check my client's 500 handling" test.

The consequence to keep in mind when reading the runtime: a scenario that declares a
non-2xx response and a request that carries `Prefer: code=` can disagree, and the
header wins. That precedence is deliberate.
