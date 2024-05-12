import CoreLocation
import FerrostarCoreFFI
import SnapshotTesting
import XCTest
@testable import FerrostarCore

final class ValhallaCoreTests: XCTestCase {
    @MainActor
    func testValhallaRouteParsing() async throws {
        let mockSession = MockURLSession()
        mockSession.registerMock(
            forURL: valhallaEndpointUrl,
            withData: sampleRouteData,
            andResponse: successfulJSONResponse
        )

        let core = try FerrostarCore(
            valhallaEndpointUrl: valhallaEndpointUrl,
            profile: "auto",
            locationProvider: SimulatedLocationProvider(),
            networkSession: mockSession
        )
        let routes = try await core.getRoutes(
            initialLocation: UserLocation(
                latitude: 60.5347155,
                longitude: -149.543469,
                horizontalAccuracy: 0,
                course: 0,
                courseAccuracy: 0,
                timestamp: Date()
            ),
            waypoints: [Waypoint(coordinate: GeographicCoordinate(lat: 60.5349908, lng: -149.5485806), kind: .break)]
        )

        assertSnapshot(of: routes, as: .dump)
    }
}
