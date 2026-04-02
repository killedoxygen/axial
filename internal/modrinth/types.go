package modrinth

// Project is a subset of the Modrinth project response.
type Project struct {
	ID          string `json:"id"`
	Slug        string `json:"slug"`
	Title       string `json:"title"`
	Description string `json:"description"`
}

// Version is a subset of the Modrinth version response.
type Version struct {
	ID            string        `json:"id"`
	ProjectID     string        `json:"project_id"`
	Name          string        `json:"name"`
	VersionNumber string        `json:"version_number"`
	GameVersions  []string      `json:"game_versions"`
	Loaders       []string      `json:"loaders"`
	Featured      bool          `json:"featured"`
	DatePublished string        `json:"date_published"`
	Files         []VersionFile `json:"files"`
}

// VersionFile describes a downloadable artifact within a Version.
type VersionFile struct {
	URL      string            `json:"url"`
	Filename string            `json:"filename"`
	Primary  bool              `json:"primary"`
	Hashes   map[string]string `json:"hashes"`
	Size     int64             `json:"size"`
}

// PrimaryFile returns the primary VersionFile, or the first file if none is marked primary.
func (v *Version) PrimaryFile() *VersionFile {
	if v == nil || len(v.Files) == 0 {
		return nil
	}
	for i := range v.Files {
		if v.Files[i].Primary {
			return &v.Files[i]
		}
	}
	return &v.Files[0]
}
