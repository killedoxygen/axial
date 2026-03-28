package system

// TotalMemoryMB returns the total physical RAM in megabytes.
func TotalMemoryMB() (int, error) {
	bytes, err := totalMemoryBytes()
	if err != nil {
		return 0, err
	}
	return int(bytes / (1024 * 1024)), nil
}

// RecommendedMemoryRange returns the min and max recommended game memory in MB
// based on total system RAM.
func RecommendedMemoryRange(totalMB int) (minMB, maxMB int) {
	quarter := totalMB / 4
	half := totalMB / 2

	minMB = quarter
	if minMB < 2048 {
		minMB = 2048
	}
	if minMB > 4096 {
		minMB = 4096
	}

	maxMB = half
	if maxMB < 4096 {
		maxMB = 4096
	}
	if maxMB > 8192 {
		maxMB = 8192
	}

	// Don't recommend more than available
	if maxMB > totalMB-2048 {
		maxMB = totalMB - 2048
	}
	if minMB > maxMB {
		minMB = maxMB
	}
	// Absolute floor
	if minMB < 1024 {
		minMB = 1024
	}

	return minMB, maxMB
}
