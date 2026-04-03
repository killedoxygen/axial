package composition

import (
	"fmt"
	"log"
	"regexp"
	"strconv"
	"strings"
)

var (
	releaseVersionPattern  = regexp.MustCompile(`^(\d+)\.(\d+)(?:\.(\d+))?$`)
	snapshotVersionPattern = regexp.MustCompile(`^\d+w\d+[a-z]$`)
)

// MCVersion is a comparable Minecraft version.
type MCVersion struct {
	Major      int
	Minor      int
	Patch      int
	IsSnapshot bool
	Raw        string
}

// Parse parses a Minecraft version string into an MCVersion.
func Parse(s string) (MCVersion, error) {
	s = strings.TrimSpace(s)
	if s == "" {
		return MCVersion{}, fmt.Errorf("empty version")
	}
	if snapshotVersionPattern.MatchString(strings.ToLower(s)) {
		return MCVersion{IsSnapshot: true, Raw: s}, nil
	}
	match := releaseVersionPattern.FindStringSubmatch(s)
	if match == nil {
		return MCVersion{}, fmt.Errorf("invalid minecraft version: %s", s)
	}

	major, _ := strconv.Atoi(match[1])
	minor, _ := strconv.Atoi(match[2])
	patch := 0
	if match[3] != "" {
		patch, _ = strconv.Atoi(match[3])
	}

	return MCVersion{
		Major: major,
		Minor: minor,
		Patch: patch,
		Raw:   s,
	}, nil
}

// Compare returns -1, 0, or 1.
func (a MCVersion) Compare(b MCVersion) int {
	if a.IsSnapshot && !b.IsSnapshot {
		return 1
	}
	if !a.IsSnapshot && b.IsSnapshot {
		return -1
	}
	if a.IsSnapshot && b.IsSnapshot {
		return strings.Compare(strings.ToLower(a.Raw), strings.ToLower(b.Raw))
	}

	if a.Major != b.Major {
		if a.Major < b.Major {
			return -1
		}
		return 1
	}
	if a.Minor != b.Minor {
		if a.Minor < b.Minor {
			return -1
		}
		return 1
	}
	if a.Patch != b.Patch {
		if a.Patch < b.Patch {
			return -1
		}
		return 1
	}
	return 0
}

// InRange returns true if v satisfies the range string.
func (v MCVersion) InRange(rangeStr string) bool {
	rangeStr = strings.TrimSpace(rangeStr)
	if rangeStr == "" {
		return true
	}
	for _, condition := range strings.Fields(rangeStr) {
		op, versionStr := splitRangeCondition(condition)
		target, err := Parse(versionStr)
		if err != nil {
			log.Printf("composition version range parse failed: range=%q condition=%q err=%v", rangeStr, condition, err)
			return false
		}
		cmp := v.Compare(target)
		switch op {
		case ">":
			if cmp <= 0 {
				return false
			}
		case ">=":
			if cmp < 0 {
				return false
			}
		case "<":
			if cmp >= 0 {
				return false
			}
		case "<=":
			if cmp > 0 {
				return false
			}
		case "=":
			if cmp != 0 {
				return false
			}
		default:
			return false
		}
	}
	return true
}

func splitRangeCondition(condition string) (string, string) {
	for _, op := range []string{">=", "<=", ">", "<", "="} {
		if strings.HasPrefix(condition, op) {
			return op, strings.TrimSpace(strings.TrimPrefix(condition, op))
		}
	}
	return "=", condition
}
