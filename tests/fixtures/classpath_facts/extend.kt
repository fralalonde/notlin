package sample

import java.time.Duration

// `plusDays` and `plus` are not in the fallback signature table: they can only
// be established from classpath facts. The member list the tests serve comes
// from `duration_members.json`, which is the helper's answer for:
//   echo '{"version":1,"id":0,"query":{"ClassMembers":{"owner":"java.time.Duration"}}}' \
//     | NOTLIN_CLASSPATH= java -jar jvm-facts/build/notlin-jvm-facts.jar
fun extend(base: Duration): Duration = base.plusDays(2L)

fun addOther(base: Duration, other: Duration): Duration = base.plus(other)
