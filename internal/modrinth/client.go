package modrinth

import (
	"context"
	"crypto/sha512"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"path"
	"slices"
	"sort"
	"strings"
	"time"
)

const userAgent = "croopor/0.3.1 (github.com/mateoltd/croopor)"

// Client is the Modrinth API client. Use NewClient to construct.
type Client interface {
	GetProject(ctx context.Context, idOrSlug string) (*Project, error)
	ListVersions(ctx context.Context, projectID string, gameVersions []string, loaders []string) ([]Version, error)
	DownloadFile(ctx context.Context, url string, sha512 string, dst io.Writer) error
}

type httpClient struct {
	*http.Client
	limiter *Limiter
	baseURL string
	apiKey  string
}

// NewClient returns a Client backed by a real HTTP connection.
func NewClient(baseURL string, apiKey string, client *http.Client) Client {
	if baseURL == "" {
		baseURL = "https://api.modrinth.com"
	}
	if client == nil {
		client = &http.Client{Timeout: 30 * time.Second}
	}
	return &httpClient{
		Client:  client,
		limiter: NewLimiter(5),
		baseURL: strings.TrimRight(baseURL, "/"),
		apiKey:  apiKey,
	}
}

func (c *httpClient) GetProject(ctx context.Context, idOrSlug string) (*Project, error) {
	var project Project
	if err := c.getJSON(ctx, "/v2/project/"+url.PathEscape(idOrSlug), nil, &project); err != nil {
		return nil, err
	}
	return &project, nil
}

func (c *httpClient) ListVersions(ctx context.Context, projectID string, gameVersions []string, loaders []string) ([]Version, error) {
	query := url.Values{}
	if len(gameVersions) > 0 {
		encoded, err := json.Marshal(gameVersions)
		if err != nil {
			return nil, err
		}
		query.Set("game_versions", string(encoded))
	}
	if len(loaders) > 0 {
		encoded, err := json.Marshal(loaders)
		if err != nil {
			return nil, err
		}
		query.Set("loaders", string(encoded))
	}

	var versions []Version
	if err := c.getJSON(ctx, "/v2/project/"+url.PathEscape(projectID)+"/version", query, &versions); err != nil {
		return nil, err
	}

	candidates := compatibleVersions(versions, gameVersions, loaders)
	if len(candidates) == 0 {
		return nil, nil
	}

	sort.SliceStable(candidates, func(i, j int) bool {
		return compareVersionPreference(candidates[i], candidates[j], gameVersions, loaders) < 0
	})
	return candidates, nil
}

func (c *httpClient) DownloadFile(ctx context.Context, rawURL string, expectedSHA512 string, dst io.Writer) error {
	ctx, cancel := withDefaultTimeout(ctx, 120*time.Second)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, rawURL, nil)
	if err != nil {
		return err
	}

	resp, err := c.do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("modrinth download %s: HTTP %d", rawURL, resp.StatusCode)
	}

	h := sha512.New()
	if _, err := io.Copy(dst, io.TeeReader(resp.Body, h)); err != nil {
		return err
	}
	if expectedSHA512 != "" {
		actual := hex.EncodeToString(h.Sum(nil))
		if !strings.EqualFold(actual, expectedSHA512) {
			return fmt.Errorf("hash mismatch: expected %s got %s", expectedSHA512, actual)
		}
	}
	return nil
}

func (c *httpClient) getJSON(ctx context.Context, endpoint string, query url.Values, dst any) error {
	ctx, cancel := withDefaultTimeout(ctx, 30*time.Second)
	defer cancel()

	u, err := url.Parse(c.baseURL)
	if err != nil {
		return err
	}
	u.Path = path.Join(u.Path, endpoint)
	if len(query) > 0 {
		u.RawQuery = query.Encode()
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, u.String(), nil)
	if err != nil {
		return err
	}

	resp, err := c.do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		body, _ := io.ReadAll(io.LimitReader(resp.Body, 4096))
		return fmt.Errorf("modrinth API %s: HTTP %d: %s", u.String(), resp.StatusCode, strings.TrimSpace(string(body)))
	}

	return json.NewDecoder(resp.Body).Decode(dst)
}

func (c *httpClient) do(req *http.Request) (*http.Response, error) {
	if err := c.limiter.Wait(req.Context()); err != nil {
		return nil, err
	}
	req.Header.Set("User-Agent", userAgent)
	if c.apiKey != "" {
		req.Header.Set("X-Ratelimit-Key", c.apiKey)
	}
	return c.Client.Do(req)
}

func withDefaultTimeout(ctx context.Context, timeout time.Duration) (context.Context, context.CancelFunc) {
	if _, ok := ctx.Deadline(); ok {
		return ctx, func() {}
	}
	return context.WithTimeout(ctx, timeout)
}

func pickBestVersion(versions []Version, gameVersions []string, loaders []string) *Version {
	candidates := compatibleVersions(versions, gameVersions, loaders)
	if len(candidates) == 0 {
		return nil
	}
	sort.SliceStable(candidates, func(i, j int) bool {
		return compareVersionPreference(candidates[i], candidates[j], gameVersions, loaders) < 0
	})
	return &candidates[0]
}

func compatibleVersions(versions []Version, gameVersions []string, loaders []string) []Version {
	candidates := make([]Version, 0, len(versions))
	for _, version := range versions {
		if !matchesAny(version.GameVersions, gameVersions) {
			continue
		}
		if !matchesAnyFold(version.Loaders, loaders) {
			continue
		}
		candidates = append(candidates, version)
	}
	return candidates
}

func compareVersionPreference(a, b Version, gameVersions []string, loaders []string) int {
	if a.Featured != b.Featured {
		if a.Featured {
			return -1
		}
		return 1
	}
	at, _ := time.Parse(time.RFC3339, a.DatePublished)
	bt, _ := time.Parse(time.RFC3339, b.DatePublished)
	if at.After(bt) {
		return -1
	}
	if bt.After(at) {
		return 1
	}
	return 0
}

func matchesAny(have []string, want []string) bool {
	if len(want) == 0 {
		return true
	}
	for _, candidate := range want {
		if slices.Contains(have, candidate) {
			return true
		}
	}
	return false
}

func matchesAnyFold(have []string, want []string) bool {
	if len(want) == 0 {
		return true
	}
	for _, candidate := range want {
		for _, existing := range have {
			if strings.EqualFold(existing, candidate) {
				return true
			}
		}
	}
	return false
}
